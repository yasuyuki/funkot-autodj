//! Two-deck mixing engine and bar-grid transition scheduler.
//!
//! Pull-based: a loader thread prepares tracks offline; [`Engine::render`] only
//! mixes pre-rendered buffers and never blocks.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::analysis::refine_periodic_phase;
use crate::filter::StereoHighPass;
use crate::stretch::{self, position_scale};
use crate::{
    cache, decode, EngineOptions, Error, Result, BEATS_PER_BAR, MAIN_GAP_BARS, MAX_SOLO_INTRO_BARS,
};

/// Beats scored by [`refine_output_downbeat`] (±half-beat periodic refine).
const PHASE_REFINE_BEATS: u32 = 12;

/// Events emitted by the engine (and loader failures).
#[derive(Debug, Clone)]
pub enum EngineEvent {
    TrackStarted { index: usize, path: PathBuf },
    TransitionStarted { from: PathBuf, to: PathBuf },
    TrackFailed { path: PathBuf, message: String },
    Finished,
}

/// Offline-rendered track ready for the mix bus.
///
/// Samples are interleaved stereo f32 at [`EngineOptions::output_sample_rate`],
/// already time-stretched so intro BPM ≈ [`EngineOptions::target_bpm`].
/// Analysis frame indices are mapped with
/// [`crate::stretch::position_scale`] `(out_frames / in_frames)`.
#[derive(Debug, Clone)]
pub struct PreparedTrack {
    pub path: PathBuf,
    pub playlist_index: usize,
    /// Interleaved stereo; shared so render never copies the buffer.
    pub samples: Arc<Vec<f32>>,
    pub frames: u64,
    /// First downbeat in the stretched/output domain.
    pub first_downbeat_out: u64,
    /// Outro start downbeat in the stretched/output domain.
    pub outro_start_out: u64,
    pub intro_bars: u32,
    pub outro_bars: u32,
    /// Linear gain from analysis (`10^(gain_db/20)`), or 1.0 when disabled.
    pub gain_linear: f32,
}

/// Pure transition schedule in bars relative to T = previous track's outro start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransitionPlan {
    pub f_eff: u32,
    /// Bars from T until the next track's main section starts.
    pub m: u32,
    /// Bars of the next track's intro skipped before entry.
    pub skip: u32,
    pub fadeout_start: u32,
    pub fadeout_end: u32,
}

/// Compute the bar-grid transition schedule.
///
/// `F` = requested fade bars, `I` = next intro bars, `O` = previous outro bars.
pub fn plan_transition(fade_bars: u32, intro_bars: u32, outro_bars: u32) -> TransitionPlan {
    let f = fade_bars;
    let i = intro_bars;
    let o = outro_bars;

    let f_eff = {
        let from_intro = i.saturating_sub(MAIN_GAP_BARS) / 2;
        let from_outro = o / 2;
        f.min(from_intro).min(from_outro).max(1)
    };

    let mut m = (2 * f_eff + MAIN_GAP_BARS).max(i.min(o.saturating_add(MAX_SOLO_INTRO_BARS)));
    m = m.min(i);
    let skip = i.saturating_sub(m);

    // fadeout_end = M - MAIN_GAP_BARS (intro remaining after prev fade-out completes).
    let fadeout_end = (m.saturating_sub(MAIN_GAP_BARS)).min(o);
    // Keep fade-out from starting before fade-in ends, but never past fadeout_end
    // (short intros with MAIN_GAP_BARS can yield fadeout_end == 0).
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

/// Linear fade-in gain over `n` frames at frame index `i` (`i` in `0..n`).
///
/// Uses the full span with bit-exact endpoints: `i == 0` → `0.0`,
/// `i == n - 1` → `1.0`. Denominator is `n - 1` so every frame participates.
/// `n == 0` or `n == 1` → instant full (`1.0`).
#[inline]
pub fn fade_in_gain(i: u64, n: u64) -> f32 {
    if n <= 1 {
        return 1.0;
    }
    if i == 0 {
        return 0.0;
    }
    if i >= n - 1 {
        return 1.0;
    }
    (i as f32) / ((n - 1) as f32)
}

/// Linear fade-out gain over `n` frames at frame index `i` (`i` in `0..n`).
///
/// Uses the full span with bit-exact endpoints: `i == 0` → `1.0`,
/// `i == n - 1` → `0.0`. `n == 0` or `n == 1` → instant silent (`0.0`).
#[inline]
pub fn fade_out_gain(i: u64, n: u64) -> f32 {
    if n <= 1 {
        return 0.0;
    }
    if i == 0 {
        return 1.0;
    }
    if i >= n - 1 {
        return 0.0;
    }
    1.0 - (i as f32) / ((n - 1) as f32)
}

#[inline]
fn bar_to_frames(bars: u32, bar_frames: f64) -> u64 {
    (f64::from(bars) * bar_frames).round() as u64
}

/// Hop size for transition phase-lock energy envelopes.
const PHASE_ALIGN_HOP: usize = 256;
/// Compare this many bars of kick-band energy when locking next entry to prev.
const PHASE_ALIGN_BARS: u32 = 4;
/// Micro-adjust half-width (fraction of one beat). Whole-beat offsets are NOT
/// searched: Funkot intros/outros put kicks on every beat, so ±1/±2 beat peaks
/// are ambiguous and steal bar identity from the analysis markers.
const PHASE_ALIGN_FINE_BEATS: f64 = 0.5;
/// Ignore the adjustment when the best normalized correlation is below this.
const PHASE_ALIGN_MIN_CORR: f64 = 0.45;

/// Micro-align `next_entry` to the previous outro's kick-band energy phase.
///
/// Only searches within ±[`PHASE_ALIGN_FINE_BEATS`] of the marker-derived
/// entry. Bar identity stays with analysis markers (file-end outro / intro
/// downbeat); kick-energy xcorr cannot resolve mod-4 bar phase on machine
/// rhythm and previously shifted 2→3 by a full beat (beats locked, bars wrong).
/// Rejects edge-clamped adjustments that would slam entry to 0.
pub fn align_next_entry_to_prev(
    prev_interleaved: &[f32],
    prev_start: u64,
    next_interleaved: &[f32],
    next_entry: u64,
    sample_rate: u32,
    beat_frames: f64,
) -> u64 {
    if sample_rate == 0 || !(beat_frames.is_finite() && beat_frames > 1.0) {
        return next_entry;
    }
    let prev_frames = prev_interleaved.len() / 2;
    let next_frames = next_interleaved.len() / 2;
    if prev_frames < PHASE_ALIGN_HOP * 4 || next_frames < PHASE_ALIGN_HOP * 4 {
        return next_entry;
    }

    let hops_per_beat = beat_frames / PHASE_ALIGN_HOP as f64;
    if !(hops_per_beat.is_finite() && hops_per_beat > 1.0) {
        return next_entry;
    }
    // Keep the lag search within ±PHASE_ALIGN_FINE_BEATS (strictly, in quantized hops).
    // Previously we used `ceil(...) + 2`, which could slightly exceed half a beat and
    // make bar identity unstable when the nominal entry starts exactly on a downbeat.
    let max_lag_hops = (PHASE_ALIGN_FINE_BEATS * hops_per_beat).floor() as usize;
    let win_hops = ((PHASE_ALIGN_BARS as f64) * f64::from(BEATS_PER_BAR) * hops_per_beat)
        .round()
        .max(16.0) as usize;
    let need = win_hops + max_lag_hops;
    let need_frames = need * PHASE_ALIGN_HOP;

    let avail_prev = prev_frames.saturating_sub(prev_start as usize);
    let avail_next = next_frames.saturating_sub(next_entry as usize);
    let avail = avail_prev.min(avail_next);
    let (win_hops, need_frames) = if avail >= need_frames {
        (win_hops, need_frames)
    } else {
        let min_lag_frames = max_lag_hops * PHASE_ALIGN_HOP;
        let min_win_frames = (2.0 * beat_frames).round() as usize;
        if avail < min_lag_frames + min_win_frames {
            return next_entry;
        }
        let w_frames = avail - min_lag_frames;
        let w_hops = (w_frames / PHASE_ALIGN_HOP).max(8);
        (w_hops, (w_hops + max_lag_hops) * PHASE_ALIGN_HOP)
    };

    let prev_mono = mono_slice(prev_interleaved, prev_start as usize, need_frames);
    let next_mono = mono_slice(next_interleaved, next_entry as usize, need_frames);
    if prev_mono.len() < need_frames || next_mono.len() < need_frames {
        return next_entry;
    }

    let prev_env = kick_energy_envelope(&prev_mono, sample_rate, PHASE_ALIGN_HOP);
    let next_env = kick_energy_envelope(&next_mono, sample_rate, PHASE_ALIGN_HOP);
    let need_hops = need_frames / PHASE_ALIGN_HOP;
    if prev_env.len() < need_hops || next_env.len() < need_hops {
        return next_entry;
    }

    let xcorr = |lag: i64| energy_xcorr_at_win(&prev_env, &next_env, lag, win_hops);
    let fine_radius = max_lag_hops as i64;
    let mut lag_hops = 0i64;
    let mut fine_corr = xcorr(0);
    for lag in -fine_radius..=fine_radius {
        let c = xcorr(lag);
        if c > fine_corr {
            fine_corr = c;
            lag_hops = lag;
        }
    }
    if !(fine_corr.is_finite() && fine_corr >= PHASE_ALIGN_MIN_CORR) {
        return next_entry;
    }

    let lag_frames = lag_hops.saturating_mul(PHASE_ALIGN_HOP as i64);
    let adjusted = if lag_frames >= 0 {
        next_entry.saturating_sub(lag_frames as u64)
    } else {
        next_entry.saturating_add((-lag_frames) as u64)
    };
    let adjusted = adjusted.min(next_frames.saturating_sub(1) as u64);

    if lag_frames > 0 && adjusted == 0 && next_entry > 0 {
        return next_entry;
    }
    if lag_frames < 0 && adjusted + 1 >= next_frames as u64 && next_entry + 1 < next_frames as u64 {
        return next_entry;
    }

    adjusted
}

fn mono_slice(interleaved: &[f32], start: usize, n: usize) -> Vec<f32> {
    let frames = interleaved.len() / 2;
    let mut out = Vec::with_capacity(n.min(frames.saturating_sub(start)));
    for i in 0..n {
        let f = start + i;
        if f >= frames {
            break;
        }
        out.push(0.5 * (interleaved[f * 2] + interleaved[f * 2 + 1]));
    }
    out
}

fn kick_energy_envelope(mono: &[f32], sample_rate: u32, hop: usize) -> Vec<f64> {
    let filtered = biquad_lowpass_mono(mono, sample_rate, 150.0);
    let mut out = Vec::new();
    let mut i = 0;
    while i + hop <= filtered.len() {
        let mut e = 0.0f64;
        for v in &filtered[i..i + hop] {
            let x = f64::from(*v);
            e += x * x;
        }
        out.push(e.sqrt());
        i += hop;
    }
    out
}

fn biquad_lowpass_mono(mono: &[f32], sample_rate: u32, cutoff_hz: f32) -> Vec<f32> {
    let sr = f64::from(sample_rate).max(1.0);
    let fc = f64::from(cutoff_hz).clamp(1.0, sr * 0.49);
    let w0 = std::f64::consts::TAU * fc / sr;
    let cos_w0 = w0.cos();
    let sin_w0 = w0.sin();
    let q = std::f64::consts::FRAC_1_SQRT_2;
    let alpha = sin_w0 / (2.0 * q);
    let b0 = ((1.0 - cos_w0) * 0.5) / (1.0 + alpha);
    let b1 = (1.0 - cos_w0) / (1.0 + alpha);
    let b2 = b0;
    let a1 = (-2.0 * cos_w0) / (1.0 + alpha);
    let a2 = (1.0 - alpha) / (1.0 + alpha);
    let mut z1 = 0.0f64;
    let mut z2 = 0.0f64;
    let mut out = Vec::with_capacity(mono.len());
    for &x in mono {
        let x = f64::from(x);
        let y = b0 * x + z1;
        z1 = b1 * x - a1 * y + z2;
        z2 = b2 * x - a2 * y;
        out.push(y as f32);
    }
    out
}

fn energy_xcorr_at_win(a: &[f64], b: &[f64], lag: i64, win: usize) -> f64 {
    if win < 4 {
        return 0.0;
    }
    let (sa, sb) = if lag >= 0 {
        let l = lag as usize;
        if l + win > a.len() || win > b.len() {
            return 0.0;
        }
        (&a[l..l + win], &b[..win])
    } else {
        let l = (-lag) as usize;
        if l + win > b.len() || win > a.len() {
            return 0.0;
        }
        (&a[..win], &b[l..l + win])
    };
    let mut corr = 0.0;
    let mut ea = 0.0;
    let mut eb = 0.0;
    for i in 0..win {
        corr += sa[i] * sb[i];
        ea += sa[i] * sa[i];
        eb += sb[i] * sb[i];
    }
    let denom = (ea * eb).sqrt().max(1e-12);
    corr / denom
}

enum LoaderMsg {
    Ready(PreparedTrack),
    Failed { path: PathBuf, message: String },
    Exhausted,
}

struct Deck {
    track: PreparedTrack,
    /// Current read position in the prepared buffer (frames).
    playhead: u64,
    highpass_enabled: bool,
    filter: StereoHighPass,
}

struct ActiveTransition {
    frames_into: u64,
    fade_in_end: u64,
    fade_out_start: u64,
    fade_out_end: u64,
}

/// Pull-based auto-DJ engine.
pub struct Engine {
    options: EngineOptions,
    bar_frames: f64,
    shutdown: Arc<AtomicBool>,
    loader_rx: Receiver<LoaderMsg>,
    /// Permits the loader to prepare another track (bounds in-flight buffers).
    permit_tx: SyncSender<()>,
    loader_join: Option<JoinHandle<()>>,
    /// Next prepared track waiting to enter a transition (at most one).
    next_track: Option<PreparedTrack>,
    active: Option<Deck>,
    prev: Option<Deck>,
    transition: Option<ActiveTransition>,
    /// Waiting for next track after crossing outro_start without a ready next.
    awaiting_next_at_outro: bool,
    outro_anchor: u64,
    pending_events: Vec<EngineEvent>,
    finished: bool,
    finished_emitted: bool,
    stopped: bool,
    /// Playlist exhausted signal from loader (no more tracks coming).
    loader_exhausted: bool,
}

impl Engine {
    pub fn new(options: EngineOptions, playlist: Vec<PathBuf>) -> Result<Self> {
        if options.output_sample_rate == 0 {
            return Err(Error::Engine("output_sample_rate must be > 0".into()));
        }
        if !(options.rate.is_finite() && options.rate > 0.0) {
            return Err(Error::Engine(format!("invalid rate: {}", options.rate)));
        }

        let bar_frames = options.bar_frames();
        let shutdown = Arc::new(AtomicBool::new(false));
        let (msg_tx, msg_rx) = mpsc::sync_channel::<LoaderMsg>(1);
        // Two permits ⇒ at most current + next prepared buffers in flight.
        let (permit_tx, permit_rx) = mpsc::sync_channel::<()>(2);
        let _ = permit_tx.try_send(());
        let _ = permit_tx.try_send(());

        let opts = options.clone();
        let shutdown_flag = Arc::clone(&shutdown);
        let join = thread::Builder::new()
            .name("funkot-loader".into())
            .spawn(move || loader_main(opts, playlist, msg_tx, permit_rx, shutdown_flag))
            .map_err(|e| Error::Engine(format!("spawn loader: {e}")))?;

        Ok(Self {
            options,
            bar_frames,
            shutdown,
            loader_rx: msg_rx,
            permit_tx,
            loader_join: Some(join),
            next_track: None,
            active: None,
            prev: None,
            transition: None,
            awaiting_next_at_outro: false,
            outro_anchor: 0,
            pending_events: Vec::new(),
            finished: false,
            finished_emitted: false,
            stopped: false,
            loader_exhausted: false,
        })
    }

    /// Fill `out` (interleaved stereo at `options.output_sample_rate`).
    /// Returns frames written; 0 means playback finished.
    /// Never blocks: outputs silence while the first track is being prepared.
    pub fn render(&mut self, out: &mut [f32]) -> usize {
        if !out.len().is_multiple_of(2) {
            return 0;
        }
        let want_frames = out.len() / 2;
        if want_frames == 0 {
            return 0;
        }

        if self.stopped || self.finished {
            return 0;
        }

        self.drain_loader();

        // Still waiting for the first track (or between tracks): silence.
        if self.active.is_none() && self.prev.is_none() {
            self.drain_loader();
            if let Some(track) = self.next_track.take() {
                self.start_first(track);
            } else if self.loader_exhausted {
                self.mark_finished();
                return 0;
            } else {
                for s in out.iter_mut() {
                    *s = 0.0;
                }
                return want_frames;
            }
        }

        let mut frames_done = 0usize;
        while frames_done < want_frames {
            if self.stopped || self.finished {
                for s in out[frames_done * 2..].iter_mut() {
                    *s = 0.0;
                }
                break;
            }

            self.drain_loader();
            self.maybe_start_or_update_transition();

            if self.active.is_none() && self.prev.is_none() {
                if self.loader_exhausted && self.next_track.is_none() {
                    self.mark_finished();
                }
                for s in out[frames_done * 2..].iter_mut() {
                    *s = 0.0;
                }
                break;
            }

            let (l, r) = self.render_one_frame();
            out[frames_done * 2] = l;
            out[frames_done * 2 + 1] = r;
            frames_done += 1;
        }

        frames_done
    }

    pub fn poll_events(&mut self) -> Vec<EngineEvent> {
        self.drain_loader();
        std::mem::take(&mut self.pending_events)
    }

    /// Frames elapsed in the current transition, if any (`0` on the first
    /// mixed transition frame). Used by tests to recover sample-accurate
    /// transition start from a multi-frame render chunk.
    pub fn transition_frames_into(&self) -> Option<u64> {
        self.transition.as_ref().map(|t| t.frames_into)
    }

    pub fn stop(&mut self) {
        self.stopped = true;
        self.shutdown.store(true, Ordering::SeqCst);
        while let Ok(msg) = self.loader_rx.try_recv() {
            if let LoaderMsg::Failed { path, message } = msg {
                self.pending_events
                    .push(EngineEvent::TrackFailed { path, message });
            }
        }
        // Unblock loader waiting on a permit.
        let _ = self.permit_tx.try_send(());
        let _ = self.permit_tx.try_send(());
        if let Some(handle) = self.loader_join.take() {
            let _ = handle.join();
        }
        self.finished = true;
    }

    fn release_permit(&self) {
        let _ = self.permit_tx.try_send(());
    }

    /// Drop the previous deck and free its loader permit exactly once.
    fn drop_prev(&mut self) {
        if self.prev.take().is_some() {
            self.release_permit();
        }
    }

    fn mark_finished(&mut self) {
        self.finished = true;
        if !self.finished_emitted {
            self.finished_emitted = true;
            self.pending_events.push(EngineEvent::Finished);
        }
    }

    fn drain_loader(&mut self) {
        loop {
            match self.loader_rx.try_recv() {
                Ok(LoaderMsg::Ready(track)) => {
                    if self.next_track.is_none() {
                        self.next_track = Some(track);
                    } else {
                        // Should not happen with the permit scheme; drop & free slot.
                        self.release_permit();
                    }
                }
                Ok(LoaderMsg::Failed { path, message }) => {
                    self.pending_events
                        .push(EngineEvent::TrackFailed { path, message });
                    // Failed prep consumed a permit; allow another attempt.
                    self.release_permit();
                }
                Ok(LoaderMsg::Exhausted) => {
                    self.loader_exhausted = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.loader_exhausted = true;
                    break;
                }
            }
        }
    }

    fn start_first(&mut self, track: PreparedTrack) {
        let path = track.path.clone();
        let index = track.playlist_index;
        let playhead = track.first_downbeat_out.min(track.frames);
        let filter = StereoHighPass::new(self.options.output_sample_rate, self.options.highpass_hz);
        self.active = Some(Deck {
            track,
            playhead,
            highpass_enabled: false,
            filter,
        });
        self.pending_events
            .push(EngineEvent::TrackStarted { index, path });
    }

    fn maybe_start_or_update_transition(&mut self) {
        if self.transition.is_some() {
            return;
        }

        let Some(active) = self.active.as_ref() else {
            return;
        };

        if self.awaiting_next_at_outro {
            self.try_begin_delayed_transition();
            return;
        }

        if active.playhead < active.track.outro_start_out {
            return;
        }

        if self.next_track.is_some() {
            self.begin_transition(0);
        } else if self.loader_exhausted {
            // Last track: play through to the buffer end.
        } else {
            self.awaiting_next_at_outro = true;
            self.outro_anchor = active.track.outro_start_out;
        }
    }

    fn try_begin_delayed_transition(&mut self) {
        if self.next_track.is_none() {
            return;
        }
        let Some(active) = self.active.as_ref() else {
            return;
        };

        let frames_past = active.playhead.saturating_sub(self.outro_anchor);
        let bars_past = (frames_past as f64 / self.bar_frames).floor() as u32;
        let next_boundary = self.outro_anchor + bar_to_frames(bars_past + 1, self.bar_frames);
        // Also allow starting exactly on a bar boundary.
        let this_boundary = self.outro_anchor + bar_to_frames(bars_past, self.bar_frames);
        if active.playhead != this_boundary && active.playhead < next_boundary {
            return;
        }

        let bars_into = if active.playhead >= next_boundary {
            bars_past + 1
        } else {
            bars_past
        };

        let o_remaining = active.track.outro_bars.saturating_sub(bars_into);
        if o_remaining < 2 {
            // Let prev finish; start next solo when active ends.
            self.awaiting_next_at_outro = false;
            return;
        }

        self.awaiting_next_at_outro = false;
        self.begin_transition(bars_into);
    }

    /// `bars_already_into_outro`: 0 for on-time start; >0 when delayed (O reduced).
    fn begin_transition(&mut self, bars_already_into_outro: u32) {
        let next = match self.next_track.take() {
            Some(t) => t,
            None => return,
        };
        let Some(active) = self.active.take() else {
            self.next_track = Some(next);
            return;
        };

        let o = active
            .track
            .outro_bars
            .saturating_sub(bars_already_into_outro);
        let plan = plan_transition(self.options.fade_bars, next.intro_bars, o);

        let nominal = next
            .first_downbeat_out
            .saturating_add(bar_to_frames(plan.skip, self.bar_frames));
        let beat_frames = self.bar_frames / f64::from(BEATS_PER_BAR);
        // Sub-beat micro-align only (±0.5 beat); bar identity stays with markers.
        let entry = align_next_entry_to_prev(
            &active.track.samples,
            active.playhead,
            &next.samples,
            nominal,
            self.options.output_sample_rate,
            beat_frames,
        );
        let entry = if next.frames == 0 {
            0
        } else {
            entry.min(next.frames - 1)
        };

        let from_path = active.track.path.clone();
        let to_path = next.path.clone();
        let next_index = next.playlist_index;

        let next_deck = Deck {
            track: next,
            playhead: entry,
            highpass_enabled: true,
            filter: StereoHighPass::new(self.options.output_sample_rate, self.options.highpass_hz),
        };

        let mut prev_deck = active;
        prev_deck.highpass_enabled = false;

        let fade_in_end = bar_to_frames(plan.f_eff, self.bar_frames);
        let fade_out_start = bar_to_frames(plan.fadeout_start, self.bar_frames);
        let fade_out_end = bar_to_frames(plan.fadeout_end, self.bar_frames);

        self.pending_events.push(EngineEvent::TransitionStarted {
            from: from_path,
            to: to_path.clone(),
        });
        self.pending_events.push(EngineEvent::TrackStarted {
            index: next_index,
            path: to_path,
        });

        self.prev = Some(prev_deck);
        self.active = Some(next_deck);
        self.transition = Some(ActiveTransition {
            frames_into: 0,
            fade_in_end,
            fade_out_start,
            fade_out_end,
        });
    }

    fn render_one_frame(&mut self) -> (f32, f32) {
        // Invariant: `prev` exists only during an active transition and only
        // while `frames_into < fade_out_end`. Any other state is stale and
        // would replay the previous track at full gain (default envelope).
        let past_fade_out = self
            .transition
            .as_ref()
            .is_some_and(|t| t.frames_into >= t.fade_out_end);
        if past_fade_out || (self.transition.is_none() && self.prev.is_some()) {
            self.drop_prev();
            if past_fade_out {
                self.transition = None;
            }
        }

        let (in_trans, frames_into, fade_in_end, fade_out_start, fade_out_end) =
            if let Some(t) = self.transition.as_ref() {
                (
                    true,
                    t.frames_into,
                    t.fade_in_end,
                    t.fade_out_start,
                    t.fade_out_end,
                )
            } else {
                (false, 0, 0, 0, 0)
            };

        let mut mix_l = 0.0f32;
        let mut mix_r = 0.0f32;

        if let Some(deck) = self.prev.as_mut() {
            // Prev is only reachable while in_trans && frames_into < fade_out_end.
            let gain_env = if frames_into >= fade_out_start {
                let span = fade_out_end.saturating_sub(fade_out_start);
                fade_out_gain(frames_into - fade_out_start, span)
            } else {
                1.0
            };
            deck.highpass_enabled = frames_into >= fade_in_end;

            // Skip the mix bus entirely at bit-exact silence (last fade-out
            // frame and any float-underflow). Playhead still advances so the
            // timeline stays consistent until we drop.
            if gain_env > 0.0 && deck.playhead < deck.track.frames {
                let (mut l, mut r) = read_deck_frame(deck);
                if deck.highpass_enabled {
                    deck.filter.process_frame(&mut l, &mut r);
                }
                let g = gain_env * deck.track.gain_linear;
                mix_l += l * g;
                mix_r += r * g;
            }
            if deck.playhead < deck.track.frames {
                deck.playhead += 1;
            }
        }

        if let Some(deck) = self.active.as_mut() {
            let gain_env = if in_trans && frames_into < fade_in_end {
                fade_in_gain(frames_into, fade_in_end)
            } else {
                1.0
            };
            if in_trans {
                deck.highpass_enabled = frames_into < fade_in_end;
            }

            // First fade-in frame is bit-exact 0: do not touch the mix bus.
            if gain_env > 0.0 && deck.playhead < deck.track.frames {
                let (mut l, mut r) = read_deck_frame(deck);
                if deck.highpass_enabled {
                    deck.filter.process_frame(&mut l, &mut r);
                }
                let g = gain_env * deck.track.gain_linear;
                mix_l += l * g;
                mix_r += r * g;
            }
            if deck.playhead < deck.track.frames {
                deck.playhead += 1;
            }
        }

        if in_trans {
            let should_drop = if let Some(t) = self.transition.as_mut() {
                t.frames_into += 1;
                // After the final zero-gain fade-out frame (`frames_into` was
                // `fade_out_end - 1`), drop `prev` and end the transition.
                t.frames_into >= t.fade_out_end
            } else {
                false
            };
            if should_drop {
                self.drop_prev();
                self.transition = None;
            }
        }

        let active_done = self
            .active
            .as_ref()
            .map(|d| d.playhead >= d.track.frames)
            .unwrap_or(true);
        if active_done && self.transition.is_none() {
            if self.active.is_some() {
                self.active = None;
                self.release_permit();
            }
            // Stale prev must never outlive the transition.
            self.drop_prev();
            self.drain_loader();
            if let Some(track) = self.next_track.take() {
                self.start_first(track);
            } else if self.loader_exhausted {
                self.mark_finished();
            }
        }

        (mix_l, mix_r)
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = self.permit_tx.try_send(());
        let _ = self.permit_tx.try_send(());
        while self.loader_rx.try_recv().is_ok() {}
        if let Some(handle) = self.loader_join.take() {
            let _ = handle.join();
        }
    }
}

fn read_deck_frame(deck: &Deck) -> (f32, f32) {
    let i = deck.playhead as usize * 2;
    let s = &deck.track.samples;
    if i + 1 < s.len() {
        (s[i], s[i + 1])
    } else {
        (0.0, 0.0)
    }
}

fn loader_main(
    options: EngineOptions,
    playlist: Vec<PathBuf>,
    tx: SyncSender<LoaderMsg>,
    permit_rx: Receiver<()>,
    shutdown: Arc<AtomicBool>,
) {
    if playlist.is_empty() {
        let _ = send_msg(&tx, LoaderMsg::Exhausted, &shutdown);
        return;
    }

    let mut rng = Rng::from_time();

    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }

        let mut order: Vec<usize> = (0..playlist.len()).collect();
        if options.random {
            fisher_yates(&mut order, &mut rng);
        }

        for &idx in &order {
            if shutdown.load(Ordering::SeqCst) {
                return;
            }

            // Wait for a free slot (current + next bound) before decoding.
            if !wait_permit(&permit_rx, &shutdown) {
                return;
            }

            let path = playlist[idx].clone();
            match prepare_one(&options, &path, idx) {
                Ok(track) => {
                    if !send_msg(&tx, LoaderMsg::Ready(track), &shutdown) {
                        return;
                    }
                }
                Err(e) => {
                    // Release the permit we consumed; engine also releases on Failed,
                    // but if send fails we must not leak. Prefer single release here
                    // by not double-freeing: engine releases on Failed recv.
                    let msg = LoaderMsg::Failed {
                        path: path.clone(),
                        message: e.to_string(),
                    };
                    if !send_msg(&tx, msg, &shutdown) {
                        return;
                    }
                }
            }
        }

        if !options.loop_playlist {
            let _ = send_msg(&tx, LoaderMsg::Exhausted, &shutdown);
            break;
        }
    }
}

fn wait_permit(permit_rx: &Receiver<()>, shutdown: &AtomicBool) -> bool {
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return false;
        }
        match permit_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(()) => return true,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => return false,
        }
    }
}

fn send_msg(tx: &SyncSender<LoaderMsg>, msg: LoaderMsg, shutdown: &AtomicBool) -> bool {
    let mut pending = Some(msg);
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return false;
        }
        let msg = match pending.take() {
            Some(m) => m,
            None => return true,
        };
        match tx.try_send(msg) {
            Ok(()) => return true,
            Err(TrySendError::Full(m)) => {
                pending = Some(m);
                thread::sleep(Duration::from_millis(50));
            }
            Err(TrySendError::Disconnected(_)) => return false,
        }
    }
}

fn prepare_one(
    options: &EngineOptions,
    path: &std::path::Path,
    index: usize,
) -> Result<PreparedTrack> {
    let buffer = decode::decode_file(path)?;
    let analysis = cache::get_or_analyze(path, &options.cache_dir, &buffer)?;

    // Stretch so intro BPM lands on target_bpm. For Funkot, outro_bpm ≈ intro_bpm.
    let intro_bpm = if analysis.intro_bpm.is_finite() && analysis.intro_bpm > 0.0 {
        analysis.intro_bpm
    } else {
        crate::NOMINAL_BPM
    };
    let speed = options.target_bpm() / intro_bpm;

    let rendered = stretch::render_track(
        &buffer.samples,
        buffer.sample_rate,
        options.output_sample_rate,
        speed,
        options.pitch_mode,
    )?;

    let in_frames = buffer.frames;
    let out_frames = (rendered.len() / 2) as u64;
    let scale = position_scale(in_frames, out_frames);
    let bar_frames = options.bar_frames();

    // Intro/outro beat *phases* are independent (middle tempo drifts). Bar
    // identity for the outro comes from the analysis marker (measured from the
    // file end), never from intro-propagated bar counting across the middle.
    let mapped_fd = (analysis.first_downbeat as f64 * scale).round() as u64;
    let mapped_outro = (analysis.outro_start as f64 * scale).round() as u64;
    let (first_downbeat_out, outro_start_out) = prepare_output_markers(
        &rendered,
        options.output_sample_rate,
        out_frames,
        mapped_fd,
        mapped_outro,
        analysis.outro_bars,
        bar_frames,
    );

    let gain_linear = if options.gain_normalize {
        let g = (10.0f64).powf(analysis.gain_db / 20.0) as f32;
        if g.is_finite() && g > 0.0 {
            g
        } else {
            1.0
        }
    } else {
        1.0
    };

    Ok(PreparedTrack {
        path: path.to_path_buf(),
        playlist_index: index,
        samples: Arc::new(rendered),
        frames: out_frames,
        first_downbeat_out: first_downbeat_out.min(out_frames.saturating_sub(1)),
        outro_start_out,
        intro_bars: analysis.intro_bars,
        outro_bars: analysis.outro_bars,
        gain_linear,
    })
}

/// Search radius for output-domain phase refine: stretch timing slack, but
/// strictly less than half a beat so we never jump to the next beat.
fn phase_search_radius_ms(beat_frames: f64, sample_rate: u32) -> f64 {
    if !(beat_frames.is_finite() && beat_frames > 0.0) || sample_rate == 0 {
        return 45.0;
    }
    let half_beat_ms = beat_frames * 1000.0 / f64::from(sample_rate) * 0.45;
    half_beat_ms.clamp(40.0, 120.0)
}

/// Refine a downbeat on offline-stretched interleaved stereo via multi-beat
/// low-band onset scoring around `approx_frame`.
pub fn refine_output_downbeat(
    interleaved_stereo: &[f32],
    sample_rate: u32,
    approx_frame: u64,
    beat_frames: f64,
) -> u64 {
    let radius_ms = phase_search_radius_ms(beat_frames, sample_rate);
    refine_periodic_phase(
        interleaved_stereo,
        sample_rate,
        approx_frame,
        beat_frames,
        PHASE_REFINE_BEATS,
        radius_ms,
    )
}

/// Map analysis markers to the stretched output domain and micro-refine only.
///
/// Single source of truth: scaled analysis `first_downbeat` / `outro_start`.
/// Each marker is nudged with [`refine_output_downbeat`] (±half beat). No
/// ±1/±2 beat searches, intro-propagated grids, or bar-phase projection.
pub fn prepare_output_markers(
    interleaved_stereo: &[f32],
    sample_rate: u32,
    out_frames: u64,
    mapped_first_downbeat: u64,
    mapped_outro: u64,
    outro_bars: u32,
    bar_frames: f64,
) -> (u64, u64) {
    let beat_frames = bar_frames / f64::from(BEATS_PER_BAR);
    let first_downbeat_out = refine_output_downbeat(
        interleaved_stereo,
        sample_rate,
        mapped_first_downbeat,
        beat_frames,
    )
    .min(out_frames.saturating_sub(1));

    let outro_start_out = derive_outro_start_out(
        interleaved_stereo,
        sample_rate,
        out_frames,
        mapped_outro,
        outro_bars,
        bar_frames,
    );

    (first_downbeat_out, outro_start_out)
}

/// Outro start from scaled analysis cache, with ±half-beat periodic refine.
///
/// When `mapped_outro` is zero (missing analysis), falls back to
/// `out_frames - outro_bars * bar_frames`.
pub fn derive_outro_start_out(
    interleaved_stereo: &[f32],
    sample_rate: u32,
    out_frames: u64,
    mapped_outro: u64,
    outro_bars: u32,
    bar_frames: f64,
) -> u64 {
    if out_frames == 0 || !(bar_frames.is_finite() && bar_frames > 0.0) {
        return 0;
    }
    let beat_frames = bar_frames / f64::from(BEATS_PER_BAR);
    let rough = if mapped_outro > 0 {
        mapped_outro
    } else {
        let outro_len = bar_to_frames(outro_bars, bar_frames);
        out_frames.saturating_sub(outro_len)
    };

    refine_output_downbeat(interleaved_stereo, sample_rate, rough, beat_frames)
        .min(out_frames.saturating_sub(1))
}

/// Legacy intro-propagated outro (coarse bar identity + diagnostics). Propagates
/// intro bar count across the whole track — sub-beat phase is wrong when the
/// middle changes tempo; use only as a coarse bar-index hint.
pub fn legacy_intro_propagated_outro(
    first_downbeat_in: u64,
    outro_start_in: u64,
    intro_bpm: f64,
    sample_rate_in: u32,
    first_downbeat_out: u64,
    bar_frames: f64,
) -> u64 {
    let bars = bars_between_markers(first_downbeat_in, outro_start_in, intro_bpm, sample_rate_in);
    let grid = first_downbeat_out.saturating_add(bar_to_frames(bars, bar_frames));
    snap_to_bar_grid(first_downbeat_out, grid, bar_frames)
}

fn bars_between_markers(
    first_downbeat: u64,
    outro_start: u64,
    intro_bpm: f64,
    sample_rate: u32,
) -> u32 {
    if outro_start <= first_downbeat
        || !(intro_bpm.is_finite() && intro_bpm > 0.0)
        || sample_rate == 0
    {
        return 0;
    }
    let bar_len = (60.0 / intro_bpm * f64::from(sample_rate) * f64::from(BEATS_PER_BAR)).max(1.0);
    let bars = (outro_start - first_downbeat) as f64 / bar_len;
    if !bars.is_finite() {
        return 0;
    }
    bars.round().clamp(0.0, f64::from(u32::MAX)) as u32
}

fn snap_to_bar_grid(anchor: u64, target: u64, bar_frames: f64) -> u64 {
    if !(bar_frames.is_finite() && bar_frames > 0.0) || target <= anchor {
        return target;
    }
    let delta = target - anchor;
    let bars = (delta as f64 / bar_frames).round().max(0.0);
    anchor.saturating_add((bars * bar_frames).round() as u64)
}

/// Tiny xorshift64 PRNG (no extra deps), seeded from `SystemTime`.
struct Rng {
    state: u64,
}

impl Rng {
    fn from_time() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0xA5A5_A5A5_A5A5_A5A5);
        Self { state: nanos | 1 }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    fn gen_index(&mut self, upper: usize) -> usize {
        if upper == 0 {
            return 0;
        }
        (self.next_u64() as usize) % upper
    }
}

fn fisher_yates(order: &mut [usize], rng: &mut Rng) {
    if order.len() < 2 {
        return;
    }
    for i in (1..order.len()).rev() {
        let j = rng.gen_index(i + 1);
        order.swap(i, j);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::refine_periodic_phase;
    use crate::testutil::synth_track;
    use crate::PitchMode;
    use std::f32::consts::PI;

    #[test]
    fn plan_worked_examples_f4_main_gap8() {
        // Default fade=4, MAIN_GAP_BARS=8.
        let p = plan_transition(4, 64, 64);
        assert_eq!(
            p,
            TransitionPlan {
                f_eff: 4,
                m: 64,
                skip: 0,
                fadeout_start: 52,
                fadeout_end: 56,
            }
        );

        let p = plan_transition(4, 64, 16);
        assert_eq!(
            p,
            TransitionPlan {
                f_eff: 4,
                m: 32,
                skip: 32,
                fadeout_start: 12,
                fadeout_end: 16,
            }
        );

        let p = plan_transition(4, 16, 64);
        assert_eq!(
            p,
            TransitionPlan {
                f_eff: 4,
                m: 16,
                skip: 0,
                fadeout_start: 4,
                fadeout_end: 8,
            }
        );

        let p = plan_transition(4, 8, 8);
        assert_eq!(
            p,
            TransitionPlan {
                f_eff: 1,
                m: 8,
                skip: 0,
                fadeout_start: 0,
                fadeout_end: 0,
            }
        );
    }

    #[test]
    fn plan_invariants_grid() {
        for &i in &[8u32, 16, 32, 64] {
            for &o in &[8u32, 16, 32, 64] {
                for f in 1u32..=8 {
                    let p = plan_transition(f, i, o);
                    assert!(p.f_eff >= 1, "f_eff I={i} O={o} F={f}");
                    assert!(p.skip <= i);
                    assert!(p.m <= i);
                    assert!(p.fadeout_start <= p.fadeout_end);
                    assert!(p.fadeout_end - p.fadeout_start <= p.f_eff);
                }
            }
        }
    }

    #[test]
    fn fade_gains_linear_at_quarters_and_endpoints() {
        // n-1 = 400 so quarter indices land on exact 0/0.25/0.5/0.75/1.
        let n = 401u64;
        let points = [
            (0u64, 0.0f32, 1.0f32),
            (100, 0.25, 0.75),
            (200, 0.5, 0.5),
            (300, 0.75, 0.25),
            (400, 1.0, 0.0),
        ];
        for &(i, expect_in, expect_out) in &points {
            let gin = fade_in_gain(i, n);
            let gout = fade_out_gain(i, n);
            assert!(
                (gin - expect_in).abs() < 1e-5,
                "fade-in i={i}: got {gin} expect {expect_in}"
            );
            assert!(
                (gout - expect_out).abs() < 1e-5,
                "fade-out i={i}: got {gout} expect {expect_out}"
            );
        }
        // Bit-exact endpoints (not merely near).
        assert_eq!(fade_in_gain(0, n), 0.0);
        assert_eq!(fade_in_gain(n - 1, n), 1.0);
        assert_eq!(fade_out_gain(0, n), 1.0);
        assert_eq!(fade_out_gain(n - 1, n), 0.0);
        // n=0 / n=1: safe instant complete.
        assert_eq!(fade_in_gain(0, 0), 1.0);
        assert_eq!(fade_in_gain(0, 1), 1.0);
        assert_eq!(fade_out_gain(0, 0), 0.0);
        assert_eq!(fade_out_gain(0, 1), 0.0);
    }

    /// Build interleaved stereo with intro kicks, a silence gap that shifts phase,
    /// then outro kicks. Outro downbeats are deliberately off the intro bar grid.
    /// Optional once-per-bar offbeat accent (does not change kick phase).
    fn synth_phase_shifted_middle(
        sr: u32,
        bpm: f64,
        intro_bars: u32,
        outro_bars: u32,
        gap_beats: f64,
    ) -> (Vec<f32>, u64, u64, f64, f64) {
        synth_phase_shifted_middle_with_accent(sr, bpm, intro_bars, outro_bars, gap_beats, false)
    }

    fn synth_phase_shifted_middle_with_accent(
        sr: u32,
        bpm: f64,
        intro_bars: u32,
        outro_bars: u32,
        gap_beats: f64,
        bar_accent: bool,
    ) -> (Vec<f32>, u64, u64, f64, f64) {
        let beat = f64::from(sr) * 60.0 / bpm;
        let bar = beat * f64::from(BEATS_PER_BAR);
        let kick_len = ((0.080 * f64::from(sr)) as usize).max(1);
        let intro_frames = (f64::from(intro_bars) * bar).round() as usize;
        let gap_frames = (gap_beats * beat).round() as usize;
        let outro_frames = (f64::from(outro_bars) * bar).round() as usize;
        let total = intro_frames + gap_frames + outro_frames;
        let mut mono = vec![0.0f32; total];

        let add_kick = |buf: &mut [f32], start: usize| {
            let end = (start + kick_len).min(buf.len());
            let tau = 0.020f64;
            for (i, frame) in (start..end).enumerate() {
                let t = i as f64 / f64::from(sr);
                let env = (-t / tau).exp() as f32;
                buf[frame] += 0.8 * env * (2.0 * PI * 60.0 * t as f32).sin();
            }
        };
        // Once-per-bar mid accent on beat index 2 (off the downbeat).
        let add_accent = |buf: &mut [f32], start: usize| {
            let end = (start + kick_len / 2).min(buf.len());
            let tau = 0.012f64;
            for (i, frame) in (start..end).enumerate() {
                let t = i as f64 / f64::from(sr);
                let env = (-t / tau).exp() as f32;
                buf[frame] += 0.45 * env * (2.0 * PI * 900.0 * t as f32).sin();
            }
        };

        let intro_beats = intro_bars * BEATS_PER_BAR;
        for b in 0..intro_beats {
            add_kick(&mut mono, (f64::from(b) * beat).round() as usize);
            if bar_accent && b % BEATS_PER_BAR == 2 {
                add_accent(&mut mono, (f64::from(b) * beat).round() as usize);
            }
        }
        let outro_start = (intro_frames + gap_frames) as u64;
        let outro_beats = outro_bars * BEATS_PER_BAR;
        for b in 0..outro_beats {
            let start = outro_start as usize + (f64::from(b) * beat).round() as usize;
            if start < mono.len() {
                add_kick(&mut mono, start);
                if bar_accent && b % BEATS_PER_BAR == 2 {
                    add_accent(&mut mono, start);
                }
            }
        }

        let mut stereo = Vec::with_capacity(total * 2);
        for &s in &mono {
            stereo.push(s);
            stereo.push(s);
        }
        (stereo, 0, outro_start, bar, beat)
    }

    #[test]
    fn variable_middle_analysis_marker_refines_not_intro_grid() {
        let sr = 44_100u32;
        let bpm = 198.0;
        let (stereo, fd, true_outro, bar, _beat) = synth_phase_shifted_middle(sr, bpm, 8, 8, 1.5);
        let out_frames = (stereo.len() / 2) as u64;
        let outro_bars = 8u32;

        let legacy = legacy_intro_propagated_outro(fd, true_outro, bpm, sr, fd, bar);
        let outro_out =
            derive_outro_start_out(&stereo, sr, out_frames, true_outro, outro_bars, bar);

        let err_tail =
            (outro_out as i64 - true_outro as i64).unsigned_abs() as f64 * 1000.0 / f64::from(sr);
        let err_legacy =
            (legacy as i64 - true_outro as i64).unsigned_abs() as f64 * 1000.0 / f64::from(sr);

        assert!(
            err_legacy > 50.0,
            "fixture must shift phase: legacy err {err_legacy:.1}ms (legacy={legacy} true={true_outro})"
        );
        assert!(
            err_tail < 8.0,
            "analysis marker + refine should lock outro: err {err_tail:.2}ms (got={outro_out} true={true_outro} legacy={legacy})"
        );
        let vs_legacy =
            (outro_out as i64 - legacy as i64).unsigned_abs() as f64 * 1000.0 / f64::from(sr);
        assert!(
            vs_legacy > 40.0,
            "chose intro grid ({legacy}) instead of analysis ({outro_out}); Δ={vs_legacy:.1}ms"
        );
    }

    #[test]
    fn analysis_mapped_refine_corrects_sub_beat_error() {
        let sr = 44_100u32;
        let bpm = 198.0;
        let gap_beats = 0.35;
        let (stereo, fd, true_outro, bar, beat) =
            synth_phase_shifted_middle_with_accent(sr, bpm, 8, 8, gap_beats, true);
        let out_frames = (stereo.len() / 2) as u64;
        let outro_bars = 8u32;

        let analysis_coarse = true_outro + (0.12 * beat).round() as u64;
        let outro_out = derive_outro_start_out(
            &stereo,
            sr,
            out_frames,
            analysis_coarse,
            outro_bars,
            bar,
        );

        let kick_err_ms =
            (outro_out as i64 - true_outro as i64).unsigned_abs() as f64 * 1000.0 / f64::from(sr);
        assert!(
            kick_err_ms < 8.0,
            "refine must lock exact kick: err {kick_err_ms:.2}ms (got={outro_out} true={true_outro})"
        );

        let (fd_out, prep_outro) = prepare_output_markers(
            &stereo,
            sr,
            out_frames,
            fd,
            true_outro,
            outro_bars,
            bar,
        );
        assert!(
            fd_out < beat.round() as u64 / 2,
            "intro stays near file start"
        );
        let prep_err =
            (prep_outro as i64 - true_outro as i64).unsigned_abs() as f64 * 1000.0 / f64::from(sr);
        assert!(
            prep_err < 8.0,
            "prepare_output_markers failed: err {prep_err:.2}ms"
        );
    }

    #[test]
    fn production_ignores_intro_legacy_grid() {
        let sr = 44_100u32;
        let bpm = 198.0;
        let (stereo, fd, true_outro, bar, beat) =
            synth_phase_shifted_middle_with_accent(sr, bpm, 8, 8, 1.5, true);
        let out_frames = (stereo.len() / 2) as u64;
        let outro_bars = 8u32;

        let legacy = legacy_intro_propagated_outro(fd, true_outro, bpm, sr, fd, bar);
        let legacy_err_beats = (legacy as i64 - true_outro as i64) as f64 / beat;
        assert!(
            legacy_err_beats.abs() > 0.4,
            "fixture must desync legacy: Δbeats={legacy_err_beats:.3}"
        );

        let production = prepare_output_markers(
            &stereo,
            sr,
            out_frames,
            fd,
            true_outro,
            outro_bars,
            bar,
        )
        .1;

        let prod_err_ms =
            (production as i64 - true_outro as i64).unsigned_abs() as f64 * 1000.0 / f64::from(sr);
        assert!(
            prod_err_ms < 8.0,
            "production marker must stay on analysis outro: err {prod_err_ms:.2}ms (got={production})"
        );
        let vs_legacy_ms =
            (production as i64 - legacy as i64).unsigned_abs() as f64 * 1000.0 / f64::from(sr);
        assert!(
            vs_legacy_ms > 40.0,
            "production must not follow legacy grid: Δ={vs_legacy_ms:.1}ms"
        );
    }

    #[test]
    fn align_next_entry_micro_only_preserves_bar_identity() {
        let sr = 44_100u32;
        let bpm = 198.0;
        let beat = f64::from(sr) * 60.0 / bpm;
        let n_beats = 64u32;
        let n = (f64::from(n_beats) * beat).round() as usize;
        let kick_len = ((0.06 * f64::from(sr)) as usize).max(1);

        let make = |phase_frames: i64| {
            let mut mono = vec![0.0f32; n];
            for b in 0..n_beats {
                let start = (f64::from(b) * beat).round() as i64 + phase_frames;
                if start < 0 {
                    continue;
                }
                let start = start as usize;
                if start >= n {
                    break;
                }
                let end = (start + kick_len).min(n);
                for (i, frame) in (start..end).enumerate() {
                    let t = i as f64 / f64::from(sr);
                    let env = (-t / 0.03).exp() as f32;
                    mono[frame] += 0.9 * env * (2.0 * std::f64::consts::PI * 60.0 * t).sin() as f32;
                }
            }
            let mut stereo = Vec::with_capacity(n * 2);
            for &s in &mono {
                stereo.push(s);
                stereo.push(s);
            }
            stereo
        };

        let prev = make(0);
        let entry = (4.0 * beat).round() as u64;
        let prev_start = entry;

        // Sub-beat late → micro-correct (~+23 ms), never a whole beat.
        let late_frames = (0.023 * f64::from(sr)).round() as i64;
        let next_late = make(late_frames);
        let aligned = align_next_entry_to_prev(&prev, prev_start, &next_late, entry, sr, beat);
        let delta_ms = (aligned as i64 - entry as i64) as f64 * 1000.0 / f64::from(sr);
        assert!(
            delta_ms > 10.0 && delta_ms < 40.0,
            "expected ~+23ms entry shift, got {delta_ms:.2}ms (aligned={aligned} entry={entry})"
        );

        // Whole-beat early: markers own bar identity — must NOT jump ±1 beat.
        let early = make(-(beat.round() as i64));
        let aligned2 = align_next_entry_to_prev(&prev, prev_start, &early, entry, sr, beat);
        let delta_beats = (aligned2 as i64 - entry as i64) as f64 / beat;
        assert!(
            delta_beats.abs() < 0.5,
            "must not steal bar identity with ±1 beat jump, got {delta_beats:.3} beats"
        );

        // Near-start nominal must not slam to 0.
        let near_start = 1_500u64;
        let aligned0 =
            align_next_entry_to_prev(&prev, near_start, &next_late, near_start, sr, beat);
        assert!(aligned0 > 0, "must not clamp entry to 0 (got {aligned0})");
    }

    #[test]
    fn periodic_refine_corrects_20_to_100ms_offset() {
        let sr = 44_100u32;
        let bpm = 198.0;
        let (stereo, fd, true_outro, _bar, beat) = synth_phase_shifted_middle(sr, bpm, 4, 8, 0.0);
        // Use outro section (steady kicks) with a deliberate 20–100 ms error.
        for &err_ms in &[20.0f64, 45.0, 80.0, 100.0] {
            let err_frames = ((err_ms / 1000.0) * f64::from(sr)).round() as i64;
            let wrong = (true_outro as i64 + err_frames).max(0) as u64;
            let radius = phase_search_radius_ms(beat, sr);
            let refined = refine_periodic_phase(&stereo, sr, wrong, beat, 12, radius);
            let got_ms =
                (refined as i64 - true_outro as i64).unsigned_abs() as f64 * 1000.0 / f64::from(sr);
            assert!(
                got_ms < 6.0,
                "offset {err_ms}ms → refined err {got_ms:.2}ms (wrong={wrong} true={true_outro} got={refined})"
            );

            let wrong_neg = (true_outro as i64 - err_frames).max(0) as u64;
            let refined_neg = refine_periodic_phase(&stereo, sr, wrong_neg, beat, 12, radius);
            let got_neg = (refined_neg as i64 - true_outro as i64).unsigned_abs() as f64 * 1000.0
                / f64::from(sr);
            assert!(
                got_neg < 6.0,
                "neg offset {err_ms}ms → refined err {got_neg:.2}ms"
            );
        }
        let _ = fd;
    }

    #[test]
    fn fixed_tempo_markers_remain_on_grid() {
        let sr = 44_100u32;
        let bpm = 180.0;
        let buf = synth_track(bpm, 16, 16, 16, sr);
        let speed = 1.10;
        let rendered = stretch::render_track(&buf.samples, sr, sr, speed, PitchMode::Preserve)
            .expect("stretch");
        let out_frames = (rendered.len() / 2) as u64;
        let scale = position_scale(buf.frames, out_frames);
        let target_bpm = bpm * speed;
        let beat = f64::from(sr) * 60.0 / target_bpm;
        let bar = beat * f64::from(BEATS_PER_BAR);
        let mapped_fd = (0.0 * scale).round() as u64;
        let outro_bars = 16u32;
        let mapped_outro = out_frames.saturating_sub((f64::from(outro_bars) * bar).round() as u64);

        let (fd_out, outro_out) = prepare_output_markers(
            &rendered,
            sr,
            out_frames,
            mapped_fd,
            mapped_outro,
            outro_bars,
            bar,
        );

        // Intro near start; outro near end − outro_bars.
        let fd_ms = fd_out as f64 * 1000.0 / f64::from(sr);
        assert!(fd_ms < 30.0, "first downbeat too late: {fd_ms:.1}ms");
        let expect_outro = out_frames.saturating_sub((f64::from(outro_bars) * bar).round() as u64);
        let err_ms =
            (outro_out as i64 - expect_outro as i64).unsigned_abs() as f64 * 1000.0 / f64::from(sr);
        assert!(
            err_ms < 15.0,
            "fixed-tempo outro off: got={outro_out} expect≈{expect_outro} err={err_ms:.1}ms"
        );

        // Outro should also be near an integer number of bars after fd at target tempo
        // for this fixed-tempo fixture (coincidence that is correct here).
        let bars = ((outro_out - fd_out) as f64 / bar).round();
        let grid = fd_out + (bars * bar).round() as u64;
        let grid_err =
            (outro_out as i64 - grid as i64).unsigned_abs() as f64 * 1000.0 / f64::from(sr);
        assert!(
            grid_err < 15.0,
            "fixed tempo should keep intro/outro bar-aligned: {grid_err:.1}ms"
        );
    }
}
