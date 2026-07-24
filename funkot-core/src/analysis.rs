//! BPM / downbeat / intro-outro analysis from head and tail segments only.
//!
//! Pipeline:
//! 1. Take the first/last ~110 s (mono).
//! 2. Build a kick-oriented onset envelope (low-band energy novelty).
//! 3. Estimate BPM via a beat-period comb filter over 172–188 BPM (0.05 step),
//!    with parabolic refinement for sub-BPM precision.
//! 4. Anchor the first downbeat at the first strong low-band energy peak,
//!    then refine to sample accuracy (hop quantization is not enough for DJ phase).
//! 5. Anchor the outro grid from the track end (production ends on a bar).
//! 6. Detect intro length via boundary contrast + edge sharpness at
//!    {8,16,32,48,64}, then medium/long cues at {48,64,80,96} (tension drop,
//!    pre-boundary fill/shout, sharp rise) when the before-window is still
//!    intro-like. A sustained RMS tension drop at 48 is checked before 64+
//!    so mid-main fill/rebuild cannot steal a true 48-bar drop intro.
//!    Prefer the earliest sustained mainization; reject later candidates when
//!    a clear earlier step already exists. Short lengths need a sharp local
//!    step (energy or spectral); gradual layering ramps must not be called
//!    short with high confidence. `bars >= 64` is not a free pass.
//!    Outro mix trigger = full mid/high-ratio drop boundary at {8,16,32,64}
//!    plus 16 bars so DJ mixing starts before the collapse (~48 bars from end
//!    on typical Funkot). Short snaps whose after-window is already a mid-outro
//!    plateau (near far-body mid/high) are rejected. Fall back to
//!    [`FALLBACK_BARS`] (64) when ambiguous.
//!    Mid-song material is never inspected.
//! 7. Reconcile intro/outro: low-confidence sides stay conservative; both
//!    high-confidence sides are kept even when `intro < outro`.
//! 8. Measure full-track mono RMS and derive a clamped gain.

use crate::cache::CACHE_VERSION;
use crate::decode::AudioBuffer;
use crate::{Error, Result, TrackAnalysis, BEATS_PER_BAR, FALLBACK_BARS, TARGET_RMS_DBFS};

/// Head/tail analysis window for BPM/downbeat only (section scan uses the full buffer).
const SEGMENT_SECS: f64 = 110.0;
/// Reject tracks shorter than this (need enough steady rhythm).
const MIN_DURATION_SECS: f64 = 30.0;
const BPM_MIN: f64 = 172.0;
const BPM_MAX: f64 = 188.0;
const HOP: usize = 256;
const LOWPASS_HZ: f64 = 150.0;
const HIGHPASS_HZ: f64 = 1500.0;
/// Onset must exceed this fraction of the segment peak to count as a beat.
const ONSET_REL_THRESH: f64 = 0.20;
/// Bars scanned for intro→main (covers 96 + after-window).
const MAX_SCAN_BARS_INTRO: usize = 112;
/// Bars scanned for main→outro (keep prior 80-bar window; longer scans
/// shift the full-drop hit and inflate outro_bars).
const MAX_SCAN_BARS_OUTRO: usize = 80;
/// Plausible Funkot outro lengths (bars). Intro snapping uses
/// [`INTRO_SNAP_CANDIDATES`] (adds 48).
const SNAP_CANDIDATES: [u32; 4] = [8, 16, 32, 64];
/// Intro snap grid including mid-length 48-bar machine intros.
const INTRO_SNAP_CANDIDATES: [u32; 5] = [8, 16, 32, 48, 64];
/// Long intro lengths using fill/shout/tension cues (earliest hit).
/// Includes 64: energy-rise scoring misses tension-drop main entries, and a
/// later 80 rebuild must not win. A 48-bar tension drop is checked first in
/// [`pick_long_intro_bars`]; non-drop 48 cues stay on the stricter medium path.
const LONG_INTRO_CANDIDATES: [u32; 3] = [64, 80, 96];
/// Bars of "main body" sampled after a candidate boundary for contrast checks.
const AFTER_WIN_BARS: usize = 8;
/// Absolute snap slack (bars) when relative tolerance alone is too tight
/// (e.g. raw outro drop at 21 → 16).
const SNAP_ABS_TOLERANCE: u32 = 6;
/// Absolute mainness floor (dB-equivalent vs early baseline) to enter the main.
const MAINNESS_ENTER: f64 = 2.0;
/// Require this many consecutive bars above a soft mainness floor.
const MAINNESS_SUSTAIN: usize = 6;
/// Onset mainness must reach this fraction of the scan-window peak.
const MAINNESS_PEAK_FRAC: f64 = 0.70;
/// Smoothing window (bars) on the mainness curve.
const MAINNESS_SMOOTH: usize = 2;
/// Max relative distance from nearest snap candidate before falling back.
const SNAP_TOLERANCE: f64 = 0.25;
/// Minimum boundary contrast at the snapped length for confidence.
const MIN_BOUNDARY_SCORE: f64 = 1.8;
/// Minimum score margin for a unique candidate winner.
const MIN_SCORE_MARGIN: f64 = 0.45;
/// Minimum edge sharpness for an 8-bar section (must be very crisp).
const MIN_SHARP_8: f64 = 3.2;
/// Minimum edge sharpness for 16/32 when the before-window is not a gradual ramp.
const MIN_SHARP_SHORT: f64 = 2.2;
/// Stricter sharpness when the before-window already ramps (layering).
const MIN_SHARP_SHORT_GRADUAL: f64 = 3.6;
/// Before-window RMS spread (dB) treated as a gradual layering ramp.
const GRADUAL_SPREAD_DB: f64 = 2.5;
/// Bars before the full energy-drop to place the DJ outro / mix trigger.
/// Real Funkot full-drop ≈ end−32; mix trigger ≈ end−48.
const OUTRO_LEAD_BARS: u32 = 16;
/// Short full-drop snaps (≤16) whose after-window sits in this fraction of the
/// far-body mid/high median are mid-outro plateaus (recovered vs floor, but not
/// yet full main). Below the band = still sparse (Sakura); at/above ~1.0 = main
/// already (classic 16-bar collapse). Reject only the mid band.
const OUTRO_MID_PLATEAU_FRAC_LO: f64 = 0.75;
const OUTRO_MID_PLATEAU_FRAC_HI: f64 = 0.98;
/// Default search radius for sample-accurate kick refinement (ms).
const KICK_REFINE_RADIUS_MS: f64 = 40.0;
/// Fine hop for kick envelope inside the refine window.
const KICK_REFINE_HOP: usize = 32;
const GAIN_CLAMP_DB: f64 = 12.0;
const SILENCE_EPS: f64 = 1e-12;

/// Per-side section length estimate before intro/outro reconciliation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SectionEstimate {
    pub bars: u32,
    pub low_confidence: bool,
    /// Boundary contrast score (higher = clearer section change).
    pub score: f64,
    /// Local edge sharpness at the candidate boundary.
    pub sharpness: f64,
}

/// Analyze a fully decoded track. `file_name` is stored for human reference.
pub fn analyze(buffer: &AudioBuffer, file_name: &str) -> Result<TrackAnalysis> {
    if buffer.sample_rate == 0 {
        return Err(Error::Analysis("sample rate is 0".into()));
    }
    if buffer.frames == 0 {
        return Err(Error::Analysis("empty audio buffer".into()));
    }

    let duration_secs = buffer.frames as f64 / buffer.sample_rate as f64;
    if duration_secs < MIN_DURATION_SECS {
        return Err(Error::Analysis(format!(
            "track too short ({duration_secs:.1}s); need at least {MIN_DURATION_SECS}s"
        )));
    }

    let sr = buffer.sample_rate as f64;
    let segment_frames = (SEGMENT_SECS * sr).round() as u64;
    let head_len = segment_frames.min(buffer.frames);
    let tail_start = buffer.frames.saturating_sub(segment_frames);
    let tail_len = buffer.frames - tail_start;

    let head = buffer.mono_range(0, head_len);
    let tail = buffer.mono_range(tail_start, tail_len);

    let head_onset = onset_envelope(&head, buffer.sample_rate, HOP)?;
    let tail_onset = onset_envelope(&tail, buffer.sample_rate, HOP)?;

    let intro_bpm = estimate_bpm(&head_onset.novelty, HOP, buffer.sample_rate)?;
    let outro_bpm = estimate_bpm(&tail_onset.novelty, HOP, buffer.sample_rate)?;

    let hop_f = HOP as f64;
    let head_beat_period_hops = bpm_to_period_hops(intro_bpm, hop_f, sr);
    let first_downbeat_hop = find_first_downbeat_hop(
        &head_onset.novelty,
        &head_onset.energy,
        head_beat_period_hops,
        HOP,
    )?;
    let first_downbeat = refine_kick_marker_mono(
        &head,
        buffer.sample_rate,
        first_downbeat_hop,
        KICK_REFINE_RADIUS_MS,
    );

    // Production convention: the file ends on a bar boundary.
    let last_bar_end = buffer.frames;
    let outro_bar_len = (60.0 / outro_bpm * sr * f64::from(BEATS_PER_BAR))
        .round()
        .max(1.0) as u64;
    let intro_bar_len = (60.0 / intro_bpm * sr * f64::from(BEATS_PER_BAR)).max(1.0);

    let intro_est =
        detect_section_bars(buffer, first_downbeat, intro_bar_len, SectionDir::Forward)?;
    let outro_est = detect_section_bars(
        buffer,
        last_bar_end,
        outro_bar_len as f64,
        SectionDir::Backward,
    )?;

    let (intro_bars, outro_bars, intro_low_conf, outro_low_conf) =
        reconcile_intro_outro(intro_est, outro_est);

    let bars_estimated_low_confidence = intro_low_conf || outro_low_conf;
    let outro_start = last_bar_end.saturating_sub(u64::from(outro_bars) * outro_bar_len);

    // Loudness: full-track mono RMS (documented).
    let (rms_dbfs, gain_db) = measure_loudness(buffer);

    let intro_bpm = finite_or_err(intro_bpm, "intro_bpm")?;
    let outro_bpm = finite_or_err(outro_bpm, "outro_bpm")?;
    let rms_dbfs = finite_or_err(rms_dbfs, "rms_dbfs")?;
    let gain_db = finite_or_err(gain_db, "gain_db")?;

    Ok(TrackAnalysis {
        version: CACHE_VERSION,
        file_name: file_name.to_string(),
        sample_rate: buffer.sample_rate,
        total_frames: buffer.frames,
        intro_bpm,
        outro_bpm,
        first_downbeat,
        outro_start,
        intro_bars,
        outro_bars,
        bars_estimated_low_confidence,
        intro_bars_low_confidence: intro_low_conf,
        outro_bars_low_confidence: outro_low_conf,
        intro_bars_manual: false,
        outro_bars_manual: false,
        needs_reanalysis: false,
        rms_dbfs,
        gain_db,
    })
}

/// Reconcile independently estimated intro/outro lengths.
///
/// Conservative rules (engine already shortens fades when intro/outro are short):
/// - Both low-confidence → [`FALLBACK_BARS`] (64) for both, both low.
/// - Intro low / outro credible → intro = `max(FALLBACK_BARS, outro)` (prefer
///   longer when ambiguous); keep intro low-confidence; preserve outro.
/// - Outro low / intro credible → outro = `min(FALLBACK_BARS, intro)` when
///   intro is the only credible side (avoid inventing a longer outro); keep
///   outro low-confidence; preserve intro.
/// - Both confident → keep both even when `intro < outro` (short-intro tracks).
///
/// Per-side confidence flags stay meaningful; the aggregate is their OR.
pub fn reconcile_intro_outro(
    intro: SectionEstimate,
    outro: SectionEstimate,
) -> (u32, u32, bool, bool) {
    if intro.low_confidence && outro.low_confidence {
        return (FALLBACK_BARS, FALLBACK_BARS, true, true);
    }

    if intro.low_confidence && !outro.low_confidence {
        let intro_bars = FALLBACK_BARS.max(outro.bars);
        return (intro_bars, outro.bars, true, false);
    }

    if outro.low_confidence && !intro.low_confidence {
        let outro_bars = FALLBACK_BARS.min(intro.bars);
        return (intro.bars, outro_bars, false, true);
    }

    // Both confident: trust each side (including intro < outro).
    (intro.bars, outro.bars, false, false)
}

/// Refine a kick/downbeat frame on interleaved stereo near `approx_frame`.
///
/// Searches ±`search_radius_ms` with a low-band energy novelty peak. Safe on
/// empty/short buffers (returns a clamped `approx_frame`). Offline only.
pub fn refine_kick_marker(
    interleaved_stereo: &[f32],
    sample_rate: u32,
    approx_frame: u64,
    search_radius_ms: f64,
) -> u64 {
    let frames = interleaved_stereo.len() / 2;
    if frames == 0 || sample_rate == 0 {
        return 0;
    }
    let approx = approx_frame.min(frames as u64 - 1);
    let radius = ((search_radius_ms.max(0.0) / 1000.0) * f64::from(sample_rate))
        .round()
        .max(1.0) as i64;
    let center = approx as i64;
    let lo = (center - radius).max(0) as usize;
    let hi = ((center + radius) as usize).min(frames.saturating_sub(1));
    if lo >= hi {
        return approx;
    }

    let mut mono = Vec::with_capacity(hi - lo + 1);
    for i in lo..=hi {
        let l = interleaved_stereo[i * 2];
        let r = interleaved_stereo[i * 2 + 1];
        mono.push(0.5 * (l + r));
    }
    let local = refine_kick_marker_mono(&mono, sample_rate, approx - lo as u64, search_radius_ms);
    (lo as u64).saturating_add(local).min(frames as u64 - 1)
}

/// Bars of sparse intro-head / outro-tail audio used by [`refine_groove_phase`].
const GROOVE_REFINE_BARS: u32 = 4;
/// Relative weight of hi-hat band vs kick when scoring Funkot bar phase.
const GROOVE_HAT_WEIGHT: f64 = 0.40;

/// Refine a downbeat by scoring kick + hi-hat placement over the first few bars.
///
/// Funkot keeps a fixed kick/hat grid inside each bar; at intro start and outro
/// end other instruments thin out, so those edges are the most reliable phase
/// anchors. Search stays inside ±half a beat (never ±1/±2 beat bar identity).
pub fn refine_groove_phase(
    interleaved_stereo: &[f32],
    sample_rate: u32,
    approx_frame: u64,
    beat_frames: f64,
    search_radius_ms: f64,
) -> u64 {
    let frames = interleaved_stereo.len() / 2;
    if frames == 0 || sample_rate == 0 || !(beat_frames.is_finite() && beat_frames > 1.0) {
        return approx_frame.min(frames.saturating_sub(1) as u64);
    }
    let approx = approx_frame.min(frames as u64 - 1);
    let half_beat_ms = beat_frames * 1000.0 / f64::from(sample_rate) * 0.45;
    let radius_ms = search_radius_ms.max(1.0).min(half_beat_ms);
    let radius = ((radius_ms / 1000.0) * f64::from(sample_rate))
        .round()
        .max(1.0) as i64;

    let n_beats = GROOVE_REFINE_BARS.saturating_mul(BEATS_PER_BAR).clamp(4, 32);
    let span = (f64::from(n_beats) * beat_frames).ceil() as i64 + radius + 8;
    let center = approx as i64;
    let lo = (center - radius).max(0) as usize;
    let hi = ((center + span) as usize).min(frames.saturating_sub(1));
    if lo >= hi {
        return approx;
    }

    let mut mono = Vec::with_capacity(hi - lo + 1);
    for i in lo..=hi {
        let l = interleaved_stereo[i * 2];
        let r = interleaved_stereo[i * 2 + 1];
        mono.push(0.5 * (l + r));
    }
    let kick = biquad_lowpass(&mono, f64::from(sample_rate), LOWPASS_HZ);
    let hat = highpass_1pole(&mono, f64::from(sample_rate), HIGHPASS_HZ);
    let hop = KICK_REFINE_HOP.min(kick.len().max(1));

    let band_peak = |band: &[f32], pos_f: f64| -> f64 {
        if pos_f < 0.0 {
            return 0.0;
        }
        let pos = pos_f.round() as isize;
        let w = (hop as isize / 2).max(1);
        let a = (pos - w).max(0) as usize;
        let b = ((pos + w) as usize).min(band.len().saturating_sub(1));
        let mut best = 0.0f64;
        for s in &band[a..=b] {
            let e = f64::from(*s).abs();
            if e.is_finite() && e > best {
                best = e;
            }
        }
        best
    };

    let approx_local = (approx as i64 - lo as i64) as f64;
    let sigma = (radius as f64 / 2.5).max(1.0);
    let mut best_delta = 0i64;
    let mut best_score = f64::NEG_INFINITY;
    let mut center_score = f64::NEG_INFINITY;
    let step = (hop as i64 / 2).max(1);
    let mut delta = -radius;
    while delta <= radius {
        let mut kick_sum = 0.0f64;
        let mut hat_sum = 0.0f64;
        for k in 0..n_beats {
            let pos = approx_local + delta as f64 + f64::from(k) * beat_frames;
            // Downbeat (every bar) weighs more; other on-beats still count.
            let w = if k % BEATS_PER_BAR == 0 {
                2.0
            } else {
                1.0
            };
            kick_sum += w * band_peak(&kick, pos);
            hat_sum += w * band_peak(&hat, pos);
            // Off-beat hats are common in Funkot; add a lighter bonus.
            let off = pos + 0.5 * beat_frames;
            hat_sum += 0.55 * w * band_peak(&hat, off);
        }
        let prior = (-0.5 * (delta as f64 / sigma).powi(2)).exp();
        let score = (kick_sum + GROOVE_HAT_WEIGHT * hat_sum) * prior;
        if delta == 0 {
            center_score = score;
        }
        if score > best_score {
            best_score = score;
            best_delta = delta;
        }
        delta += step;
    }
    if !best_score.is_finite() || best_score <= 0.0 {
        return refine_periodic_phase(
            interleaved_stereo,
            sample_rate,
            approx,
            beat_frames,
            n_beats,
            radius_ms,
        );
    }
    if best_delta.abs() > radius / 3
        && center_score.is_finite()
        && center_score > 0.0
        && best_score < center_score * 1.10
    {
        best_delta = 0;
    }

    let coarse = (approx as i64 + best_delta).max(0) as u64;
    // Snap to the nearest kick transient for sample accuracy.
    let refine_lo = (coarse as i64 - hop as i64).max(lo as i64) as usize;
    let refine_hi = ((coarse as i64 + hop as i64) as usize).min(hi);
    let mut best_frame = coarse.min(frames as u64 - 1);
    let mut best_e = f64::NEG_INFINITY;
    for frame in refine_lo..=refine_hi {
        let j = frame - lo;
        if j >= kick.len() {
            break;
        }
        let e = f64::from(kick[j]).abs();
        if e.is_finite() && e > best_e {
            best_e = e;
            best_frame = frame as u64;
        }
    }
    if !best_e.is_finite() || best_e < SILENCE_EPS {
        return coarse.min(frames as u64 - 1);
    }
    best_frame.min(frames as u64 - 1)
}

/// Refine a downbeat by scoring low-band onset energy across several following beats.
///
/// Unlike [`refine_kick_marker`] (single local peak), this chooses the phase offset
/// within `±search_radius_ms` of `approx_frame` that best aligns `n_beats` periodic
/// kicks at `beat_frames`. The search window must stay below half a beat so the
/// result remains the expected boundary, not a later beat.
pub fn refine_periodic_phase(
    interleaved_stereo: &[f32],
    sample_rate: u32,
    approx_frame: u64,
    beat_frames: f64,
    n_beats: u32,
    search_radius_ms: f64,
) -> u64 {
    let frames = interleaved_stereo.len() / 2;
    if frames == 0 || sample_rate == 0 || !(beat_frames.is_finite() && beat_frames > 1.0) {
        return approx_frame.min(frames.saturating_sub(1) as u64);
    }
    let approx = approx_frame.min(frames as u64 - 1);
    let n_beats = n_beats.clamp(2, 32);
    let half_beat_ms = beat_frames * 1000.0 / f64::from(sample_rate) * 0.45;
    let radius_ms = search_radius_ms.max(1.0).min(half_beat_ms);
    let radius = ((radius_ms / 1000.0) * f64::from(sample_rate))
        .round()
        .max(1.0) as i64;

    let span = (f64::from(n_beats) * beat_frames).ceil() as i64 + radius + 8;
    let center = approx as i64;
    let lo = (center - radius).max(0) as usize;
    let hi = ((center + span) as usize).min(frames.saturating_sub(1));
    if lo >= hi {
        return approx;
    }

    let mut mono = Vec::with_capacity(hi - lo + 1);
    for i in lo..=hi {
        let l = interleaved_stereo[i * 2];
        let r = interleaved_stereo[i * 2 + 1];
        mono.push(0.5 * (l + r));
    }
    let filtered = biquad_lowpass(&mono, f64::from(sample_rate), LOWPASS_HZ);
    let hop = KICK_REFINE_HOP.min(filtered.len().max(1));
    let n_hops = filtered.len().saturating_sub(1) / hop;
    if n_hops < 2 {
        return refine_kick_marker(interleaved_stereo, sample_rate, approx, radius_ms);
    }

    // Per-hop low-band energy + positive novelty (onset strength).
    let mut energy = Vec::with_capacity(n_hops);
    for i in 0..n_hops {
        let start = i * hop;
        let end = (start + hop).min(filtered.len());
        let mut e = 0.0f64;
        for &s in &filtered[start..end] {
            let x = f64::from(s);
            e += x * x;
        }
        energy.push(if e.is_finite() { e } else { 0.0 });
    }
    let mut novelty = vec![0.0f64; energy.len()];
    for i in 1..energy.len() {
        let d = energy[i] - energy[i - 1];
        novelty[i] = if d.is_finite() && d > 0.0 { d } else { 0.0 };
    }
    let peak = novelty.iter().copied().fold(0.0f64, f64::max);
    if !(peak.is_finite()) || peak < SILENCE_EPS {
        return refine_kick_marker(interleaved_stereo, sample_rate, approx, radius_ms);
    }

    // Sample low-band |amplitude| at a frame (local max in ±hop/2).
    let local_peak = |pos_f: f64| -> f64 {
        if pos_f < 0.0 {
            return 0.0;
        }
        let pos = pos_f.round() as isize;
        let w = (hop as isize / 2).max(1);
        let a = (pos - w).max(0) as usize;
        let b = ((pos + w) as usize).min(filtered.len().saturating_sub(1));
        let mut best = 0.0f64;
        for s in &filtered[a..=b] {
            let e = f64::from(*s).abs();
            if e.is_finite() && e > best {
                best = e;
            }
        }
        best
    };

    let novelty_at = |pos_f: f64| -> f64 {
        if pos_f < 0.0 {
            return 0.0;
        }
        let hop_i = (pos_f / hop as f64).round() as isize;
        if hop_i >= 0 && (hop_i as usize) < novelty.len() {
            let v = novelty[hop_i as usize];
            if v.is_finite() {
                return v;
            }
        }
        0.0
    };

    // Score candidate phases: multi-beat onset + energy, with a strong Gaussian
    // prior so we stay near the expected boundary (not an arbitrary louder hit).
    let approx_local = (approx as i64 - lo as i64) as f64;
    let sigma = (radius as f64 / 2.5).max(1.0);
    let mut best_delta = 0i64;
    let mut best_score = f64::NEG_INFINITY;
    let mut center_score = f64::NEG_INFINITY;
    let step = (hop as i64 / 2).max(1);
    let mut delta = -radius;
    while delta <= radius {
        let mut onset_sum = 0.0f64;
        let mut energy_sum = 0.0f64;
        for k in 0..n_beats {
            let pos = approx_local + delta as f64 + f64::from(k) * beat_frames;
            let w = if k == 0 { 2.0 } else { 1.0 };
            onset_sum += w * novelty_at(pos);
            energy_sum += w * local_peak(pos);
        }
        let prior = (-0.5 * (delta as f64 / sigma).powi(2)).exp();
        let score = (onset_sum + 0.35 * energy_sum) * prior;
        if delta == 0 {
            center_score = score;
        }
        if score > best_score {
            best_score = score;
            best_delta = delta;
        }
        delta += step;
    }
    if !best_score.is_finite() || best_score <= 0.0 {
        return refine_kick_marker(interleaved_stereo, sample_rate, approx, radius_ms);
    }
    // Reject a far jump that is only marginally better than staying put.
    if best_delta.abs() > radius / 3
        && center_score.is_finite()
        && center_score > 0.0
        && best_score < center_score * 1.12
    {
        best_delta = 0;
    }

    let coarse = (approx as i64 + best_delta).max(0) as u64;
    // Sample-accurate peak of |LPF| near the chosen phase (first beat only).
    let refine_lo = (coarse as i64 - hop as i64).max(lo as i64) as usize;
    let refine_hi = ((coarse as i64 + hop as i64) as usize).min(hi);
    let mut best_frame = coarse.min(frames as u64 - 1);
    let mut best_e = f64::NEG_INFINITY;
    for frame in refine_lo..=refine_hi {
        let j = frame - lo;
        if j >= filtered.len() {
            break;
        }
        let e = f64::from(filtered[j]).abs();
        if e.is_finite() && e > best_e {
            best_e = e;
            best_frame = frame as u64;
        }
    }
    if !best_e.is_finite() || best_e < SILENCE_EPS {
        return coarse.min(frames as u64 - 1);
    }
    best_frame.min(frames as u64 - 1)
}

/// Mono variant of [`refine_kick_marker`].
pub fn refine_kick_marker_mono(
    mono: &[f32],
    sample_rate: u32,
    approx_frame: u64,
    search_radius_ms: f64,
) -> u64 {
    if mono.is_empty() || sample_rate == 0 {
        return 0;
    }
    let approx = approx_frame.min(mono.len() as u64 - 1);
    let radius = ((search_radius_ms.max(0.0) / 1000.0) * f64::from(sample_rate))
        .round()
        .max(1.0) as i64;
    let center = approx as i64;
    let lo = (center - radius).max(0) as usize;
    let hi = ((center + radius) as usize).min(mono.len().saturating_sub(1));
    if lo >= hi {
        return approx;
    }

    let window = &mono[lo..=hi];
    let filtered = biquad_lowpass(window, f64::from(sample_rate), LOWPASS_HZ);
    let hop = KICK_REFINE_HOP.min(filtered.len().max(1));
    let n_hops = filtered.len().saturating_sub(1) / hop;
    if n_hops < 2 {
        return refine_by_abs_peak(&filtered, lo, approx);
    }

    let mut energy = Vec::with_capacity(n_hops);
    for i in 0..n_hops {
        let start = i * hop;
        let end = (start + hop).min(filtered.len());
        let mut e = 0.0f64;
        for &s in &filtered[start..end] {
            let x = f64::from(s);
            e += x * x;
        }
        if e.is_finite() {
            energy.push(e);
        } else {
            energy.push(0.0);
        }
    }

    let mut novelty = Vec::with_capacity(energy.len());
    novelty.push(0.0);
    for i in 1..energy.len() {
        let d = energy[i] - energy[i - 1];
        novelty.push(if d.is_finite() && d > 0.0 { d } else { 0.0 });
    }

    let peak = novelty.iter().copied().fold(0.0f64, f64::max);
    if !(peak.is_finite()) || peak < SILENCE_EPS {
        return refine_by_abs_peak(&filtered, lo, approx);
    }

    // Prefer the novelty peak nearest the approximate marker (weighted by strength).
    let approx_local = approx.saturating_sub(lo as u64) as f64;
    let mut best_i = 0usize;
    let mut best_val = f64::NEG_INFINITY;
    for (i, &v) in novelty.iter().enumerate() {
        if !v.is_finite() {
            continue;
        }
        let frame = i as f64 * hop as f64;
        let dist = (frame - approx_local).abs();
        let weight = v * (1.0 / (1.0 + dist / (hop as f64 * 2.0)));
        if weight > best_val {
            best_val = weight;
            best_i = i;
        }
    }

    let coarse = lo + best_i * hop;
    let refine_lo = coarse.saturating_sub(hop);
    let refine_hi = (coarse + hop).min(mono.len().saturating_sub(1));
    let mut best_frame = coarse;
    let mut best_e = f64::NEG_INFINITY;
    // Peak absolute LPF sample in the fine window (sample-accurate).
    let filt_offset = refine_lo.saturating_sub(lo);
    let filt_end = (refine_hi - lo).min(filtered.len().saturating_sub(1));
    for (j, &s) in filtered
        .iter()
        .enumerate()
        .take(filt_end + 1)
        .skip(filt_offset)
    {
        let e = f64::from(s).abs();
        if e.is_finite() && e > best_e {
            best_e = e;
            best_frame = lo + j;
        }
    }
    if !best_e.is_finite() || best_e < SILENCE_EPS {
        return approx;
    }
    (best_frame as u64).min(mono.len() as u64 - 1)
}

fn refine_by_abs_peak(filtered: &[f32], lo: usize, approx: u64) -> u64 {
    let mut best_i = 0usize;
    let mut best = f64::NEG_INFINITY;
    for (i, &s) in filtered.iter().enumerate() {
        let e = f64::from(s).abs();
        if e.is_finite() && e > best {
            best = e;
            best_i = i;
        }
    }
    if !best.is_finite() || best < SILENCE_EPS {
        approx
    } else {
        (lo + best_i) as u64
    }
}

fn finite_or_err(v: f64, name: &str) -> Result<f64> {
    if v.is_finite() {
        Ok(v)
    } else {
        Err(Error::Analysis(format!("{name} is not finite")))
    }
}

/// Onset-strength envelope: 2nd-order LPF (~150 Hz) → hop energy → half-wave Δ.
fn onset_envelope(mono: &[f32], sample_rate: u32, hop: usize) -> Result<OnsetData> {
    if mono.len() < hop * 4 {
        return Err(Error::Analysis(
            "segment too short for onset envelope".into(),
        ));
    }

    let filtered = biquad_lowpass(mono, f64::from(sample_rate), LOWPASS_HZ);
    let n_hops = filtered.len().saturating_sub(1) / hop;
    if n_hops < 8 {
        return Err(Error::Analysis("not enough hops for onset envelope".into()));
    }

    let mut energy = Vec::with_capacity(n_hops);
    for i in 0..n_hops {
        let start = i * hop;
        let end = (start + hop).min(filtered.len());
        let mut e = 0.0f64;
        for &s in &filtered[start..end] {
            let x = f64::from(s);
            e += x * x;
        }
        energy.push(e);
    }

    // Half-wave-rectified first difference (novelty) for the tempo comb filter.
    // Low-band energy is retained for first-kick detection (LPF cold-start safe).
    let mut novelty = Vec::with_capacity(energy.len());
    novelty.push(0.0);
    for i in 1..energy.len() {
        let d = energy[i] - energy[i - 1];
        novelty.push(if d > 0.0 { d } else { 0.0 });
    }

    let peak = novelty.iter().copied().fold(0.0f64, f64::max);
    if peak < SILENCE_EPS {
        return Err(Error::Analysis("no onsets found in segment".into()));
    }
    Ok(OnsetData { novelty, energy })
}

struct OnsetData {
    novelty: Vec<f64>,
    energy: Vec<f64>,
}

/// RBJ biquad low-pass, Q = 1/√2 (Butterworth).
fn biquad_lowpass(input: &[f32], sample_rate: f64, cutoff_hz: f64) -> Vec<f32> {
    if input.is_empty() || !(sample_rate.is_finite() && sample_rate > 0.0) {
        return vec![0.0; input.len()];
    }
    let w0 = 2.0 * std::f64::consts::PI * cutoff_hz / sample_rate;
    if !w0.is_finite() {
        return input.to_vec();
    }
    let cos_w0 = w0.cos();
    let sin_w0 = w0.sin();
    // Q = 1/√2 ⇒ α = sin(w0) / (2Q) = sin(w0) * √2 / 2 = sin(w0) / √2
    let alpha = sin_w0 * std::f64::consts::FRAC_1_SQRT_2;

    let b0 = (1.0 - cos_w0) / 2.0;
    let b1 = 1.0 - cos_w0;
    let b2 = (1.0 - cos_w0) / 2.0;
    let a0 = 1.0 + alpha;
    let a1 = -2.0 * cos_w0;
    let a2 = 1.0 - alpha;

    if !a0.is_finite() || a0.abs() < 1e-18 {
        return input.to_vec();
    }

    let b0 = b0 / a0;
    let b1 = b1 / a0;
    let b2 = b2 / a0;
    let a1 = a1 / a0;
    let a2 = a2 / a0;

    let mut out = vec![0.0f32; input.len()];
    let mut x1 = 0.0f64;
    let mut x2 = 0.0f64;
    let mut y1 = 0.0f64;
    let mut y2 = 0.0f64;
    for (i, &xin) in input.iter().enumerate() {
        let x0 = f64::from(xin);
        let y0 = b0 * x0 + b1 * x1 + b2 * x2 - a1 * y1 - a2 * y2;
        out[i] = if y0.is_finite() { y0 as f32 } else { 0.0 };
        x2 = x1;
        x1 = x0;
        y2 = y1;
        y1 = if y0.is_finite() { y0 } else { 0.0 };
    }
    out
}

/// 1st-order high-pass for mid/high energy measurement.
fn highpass_1pole(input: &[f32], sample_rate: f64, cutoff_hz: f64) -> Vec<f32> {
    let rc = 1.0 / (2.0 * std::f64::consts::PI * cutoff_hz);
    let dt = 1.0 / sample_rate;
    let alpha = rc / (rc + dt);
    let mut out = vec![0.0f32; input.len()];
    let mut prev_x = 0.0f64;
    let mut prev_y = 0.0f64;
    for (i, &xin) in input.iter().enumerate() {
        let x = f64::from(xin);
        let y = alpha * (prev_y + x - prev_x);
        out[i] = if y.is_finite() { y as f32 } else { 0.0 };
        prev_x = x;
        prev_y = if y.is_finite() { y } else { 0.0 };
    }
    out
}

fn bpm_to_period_hops(bpm: f64, hop: f64, sample_rate: f64) -> f64 {
    (60.0 / bpm) * sample_rate / hop
}

fn estimate_bpm(onset: &[f64], hop: usize, sample_rate: u32) -> Result<f64> {
    let sr = f64::from(sample_rate);
    let hop_f = hop as f64;
    let min_period = bpm_to_period_hops(BPM_MAX, hop_f, sr);
    let max_period = bpm_to_period_hops(BPM_MIN, hop_f, sr);

    if onset.len() < (max_period as usize) + 8 {
        return Err(Error::Analysis(
            "segment too short for BPM estimation".into(),
        ));
    }

    // Comb-filter tempo: for each BPM, take the best phase of a beat-period comb
    // on the onset novelty. Impulse-train kicks peak sharply at the true tempo.
    let step = 0.05;
    let mut best_bpm = 180.0;
    let mut best_score = f64::NEG_INFINITY;
    let mut bpm = BPM_MIN;
    while bpm <= BPM_MAX + 1e-9 {
        let period = bpm_to_period_hops(bpm, hop_f, sr);
        if period < min_period - 0.5 || period > max_period + 0.5 {
            bpm += step;
            continue;
        }
        let score = best_comb_score(onset, period);
        if score > best_score {
            best_score = score;
            best_bpm = bpm;
        }
        bpm += step;
    }

    if !best_score.is_finite() || best_score <= 0.0 {
        return Err(Error::Analysis("no usable tempo comb peak".into()));
    }

    let mid = best_bpm;
    let d = step;
    let s0 = best_comb_score(onset, bpm_to_period_hops(mid - d, hop_f, sr));
    let s1 = best_comb_score(onset, bpm_to_period_hops(mid, hop_f, sr));
    let s2 = best_comb_score(onset, bpm_to_period_hops(mid + d, hop_f, sr));
    let denom = s0 - 2.0 * s1 + s2;
    if denom.abs() > 1e-18 {
        let delta = 0.5 * (s0 - s2) / denom;
        best_bpm = mid + delta.clamp(-1.5, 1.5) * d;
    }

    if !best_bpm.is_finite() || !(BPM_MIN - 1.0..=BPM_MAX + 1.0).contains(&best_bpm) {
        return Err(Error::Analysis(format!(
            "BPM estimate out of range: {best_bpm}"
        )));
    }
    Ok(best_bpm.clamp(BPM_MIN - 0.5, BPM_MAX + 0.5))
}

/// Max over phase of the mean onset value sampled on a comb with the given period.
fn best_comb_score(onset: &[f64], period: f64) -> f64 {
    if period < 1.0 || onset.is_empty() {
        return 0.0;
    }
    let n_phases = 32;
    let mut best = f64::NEG_INFINITY;
    for p in 0..n_phases {
        let phase = period * (p as f64 / f64::from(n_phases));
        let mut sum = 0.0f64;
        let mut count = 0u32;
        let mut t = phase;
        while t < onset.len() as f64 {
            let i0 = t.floor() as usize;
            let i1 = (i0 + 1).min(onset.len() - 1);
            let frac = t - i0 as f64;
            sum += onset[i0] * (1.0 - frac) + onset[i1] * frac;
            count += 1;
            t += period;
        }
        if count > 0 {
            let mean = sum / f64::from(count);
            if mean > best {
                best = mean;
            }
        }
    }
    if best.is_finite() {
        best
    } else {
        0.0
    }
}

fn find_first_downbeat_hop(
    novelty: &[f64],
    energy: &[f64],
    period_hops: f64,
    hop: usize,
) -> Result<u64> {
    // Use low-band energy to find the first kick. Novelty can miss a kick at
    // t≈0 because the LPF starts from a zero state (cold-start transient).
    let e_peak = energy.iter().copied().fold(0.0f64, f64::max);
    if e_peak < SILENCE_EPS {
        return Err(Error::Analysis("no significant onset for downbeat".into()));
    }
    let e_thresh = e_peak * ONSET_REL_THRESH;
    let first_hop = energy
        .iter()
        .position(|&v| v >= e_thresh)
        .ok_or_else(|| Error::Analysis("no significant onset for downbeat".into()))?;

    // Refine within ±¼ beat using novelty when available; otherwise keep energy index.
    let half = (period_hops * 0.25).max(1.0);
    let lo = (first_hop as f64 - half).max(0.0) as usize;
    let hi = ((first_hop as f64 + half) as usize).min(novelty.len().saturating_sub(1));
    let mut best_hop = first_hop;
    let mut best_val = f64::NEG_INFINITY;
    if lo <= hi {
        for (i, &v) in novelty.iter().enumerate().take(hi + 1).skip(lo) {
            if v > best_val {
                best_val = v;
                best_hop = i;
            }
        }
    }
    // If novelty refine found nothing useful (all ~0 near start), stay on energy index.
    if best_val < SILENCE_EPS {
        best_hop = first_hop;
    }

    Ok((best_hop as f64 * hop as f64).round().max(0.0) as u64)
}

#[derive(Clone, Copy)]
enum SectionDir {
    Forward,
    Backward,
}

fn detect_section_bars(
    buffer: &AudioBuffer,
    anchor: u64,
    bar_len: f64,
    dir: SectionDir,
) -> Result<SectionEstimate> {
    let bar_len_frames = bar_len.round().max(1.0) as u64;
    let max_scan = match dir {
        SectionDir::Forward => MAX_SCAN_BARS_INTRO,
        SectionDir::Backward => MAX_SCAN_BARS_OUTRO,
    };
    let n_bars = match dir {
        SectionDir::Forward => {
            max_scan.min(((buffer.frames.saturating_sub(anchor)) / bar_len_frames) as usize)
        }
        SectionDir::Backward => max_scan.min((anchor / bar_len_frames) as usize),
    };
    // Need at least one snap candidate + a short after-window.
    if n_bars < SNAP_CANDIDATES[0] as usize + 4 {
        return Ok(SectionEstimate {
            bars: FALLBACK_BARS,
            low_confidence: true,
            score: 0.0,
            sharpness: 0.0,
        });
    }

    let starts: Vec<u64> = (0..n_bars)
        .map(|i| match dir {
            SectionDir::Forward => anchor + i as u64 * bar_len_frames,
            SectionDir::Backward => anchor.saturating_sub((i as u64 + 1) * bar_len_frames),
        })
        .collect();

    let feats = bar_features(buffer, &starts, bar_len_frames);
    Ok(match dir {
        SectionDir::Forward => pick_section_bars(&feats),
        SectionDir::Backward => pick_outro_bars(&feats),
    })
}

/// Outro mix-trigger length: full mid-energy drop + [`OUTRO_LEAD_BARS`].
///
/// Mixing at the collapse itself is too late; walking back 16 bars lands near
/// the mid-energy DJ outro (~end−48 when the drop is ~end−32). Falls back to
/// candidate scoring without the "prefer longer" bias used on intros.
fn pick_outro_bars(feats: &[BarFeat]) -> SectionEstimate {
    if let Some(drop) = pick_outro_full_drop(feats) {
        let can_lead = drop.bars < FALLBACK_BARS
            && feats.len()
                >= drop.bars as usize + OUTRO_LEAD_BARS as usize + AFTER_WIN_BARS;
        let bars = if can_lead {
            snap_to_bar_grid(drop.bars.saturating_add(OUTRO_LEAD_BARS)).min(FALLBACK_BARS)
        } else {
            drop.bars
        }
        .max(SNAP_CANDIDATES[0]);
        return SectionEstimate {
            bars,
            low_confidence: false,
            score: drop.score,
            sharpness: drop.sharpness,
        };
    }
    if let Some(hit) = pick_by_candidate_scores_outro(feats) {
        return hit;
    }
    SectionEstimate {
        bars: FALLBACK_BARS,
        low_confidence: true,
        score: 0.0,
        sharpness: 0.0,
    }
}

/// Full energy-drop boundary from the file end, snapped to {8,16,32,64}.
///
/// Funkot outros often keep the kick, so RMS/mid body stay up; the collapse that
/// DJs hear is mid/high content. Detect that via [`BarFeat::midhigh_ratio`]
/// (earliest sustained rise above the near-end floor). Mixing at this boundary
/// is too late — callers add [`OUTRO_LEAD_BARS`].
fn pick_outro_full_drop(feats: &[BarFeat]) -> Option<SectionEstimate> {
    if feats.len() < 16 + AFTER_WIN_BARS {
        return None;
    }

    // Skip near-silent trailing bars when sampling the outro floor.
    let mut floor_lo = 0usize;
    while floor_lo + 8 < feats.len() && amp_to_db(feats[floor_lo].rms) < -30.0 {
        floor_lo += 1;
    }
    let floor_hi = (floor_lo + 12).min(feats.len());
    if floor_hi <= floor_lo + 4 {
        return None;
    }

    // Silence pads often have unstable HF ratios; treat them as 0.
    let ratio: Vec<f64> = feats
        .iter()
        .map(|f| {
            if amp_to_db(f.rms) < -30.0 {
                0.0
            } else {
                f.midhigh_ratio.max(1e-6)
            }
        })
        .collect();

    let mut floor_vals: Vec<f64> = ratio[floor_lo..floor_hi].to_vec();
    floor_vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let floor = floor_vals[floor_vals.len() / 2];

    let far_lo = (feats.len() / 2).max(floor_hi + 4);
    if far_lo >= ratio.len() {
        return None;
    }
    let peak = ratio[far_lo..]
        .iter()
        .copied()
        .fold(0.0f64, f64::max);
    let far_hi = (far_lo + 16).min(ratio.len());
    let mut far_vals = ratio[far_lo..far_hi].to_vec();
    far_vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let far_med = far_vals[far_vals.len() / 2];
    // Need a clear mid/high recovery past the drop floor (≈0.06–0.1 → 0.15+).
    if peak < floor * 1.35 && (peak - floor) < 0.03 {
        return None;
    }
    // Light threshold: a brief layer-add spike should not wait for a long smooth.
    let enter = (floor * 1.25).max(floor + (peak - floor) * 0.25);
    let soft = enter * 0.85;
    const SUSTAIN: usize = 3;

    let last = ratio.len().saturating_sub(SUSTAIN);
    for i in floor_hi..last {
        if ratio[i] < enter {
            continue;
        }
        let win = &ratio[i..i + SUSTAIN];
        if !win.iter().all(|&v| v >= soft) {
            continue;
        }
        // Reject a single-bar spike (common mid-outro hat fills).
        let above_enter = win.iter().filter(|&&v| v >= enter).count();
        if above_enter < 2 {
            continue;
        }
        let (bars, snap_low) = snap_bars(i as u32);
        if snap_low {
            continue;
        }
        let c = bars as usize;
        if c + 4 > feats.len() {
            continue;
        }
        let after_n = AFTER_WIN_BARS.min(feats.len() - c).max(4);
        let before = &feats[..c];
        let after = &feats[c..c + after_n];
        // Short snap into a mid-outro plateau (after/far in (lo, hi)) is not the
        // main→outro collapse; keep searching. Sparse after (Sakura) and full-main
        // after (classic 16-bar floor) are kept.
        if bars <= 16 {
            let mut after_vals: Vec<f64> = ratio[c..c + after_n].to_vec();
            after_vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let after_med = after_vals[after_vals.len() / 2];
            let rel = after_med / far_med.max(1e-9);
            if after_med >= enter * 0.9
                && rel >= OUTRO_MID_PLATEAU_FRAC_LO
                && rel < OUTRO_MID_PLATEAU_FRAC_HI
            {
                continue;
            }
        }
        return Some(SectionEstimate {
            bars,
            low_confidence: false,
            score: peak / floor.max(1e-6),
            sharpness: edge_sharpness(before, after),
        });
    }
    None
}

/// Like [`pick_by_candidate_scores`] but without preferring longer near-ties
/// (that bias pushed real Funkot outros to 64).
fn pick_by_candidate_scores_outro(feats: &[BarFeat]) -> Option<SectionEstimate> {
    let mut scored: Vec<SectionEstimate> = Vec::with_capacity(SNAP_CANDIDATES.len());
    for &cand in &SNAP_CANDIDATES {
        let c = cand as usize;
        if c < 4 || c + 4 > feats.len() {
            continue;
        }
        let after_n = AFTER_WIN_BARS.min(feats.len() - c).max(4);
        let before = &feats[..c];
        let after = &feats[c..c + after_n];
        let score = boundary_score(before, after);
        let sharpness = edge_sharpness(before, after);
        if score < MIN_BOUNDARY_SCORE {
            continue;
        }
        if !passes_short_sharpness_gate(cand, before, after, sharpness) {
            continue;
        }
        scored.push(SectionEstimate {
            bars: cand,
            low_confidence: false,
            score,
            sharpness,
        });
    }
    if scored.is_empty() {
        return None;
    }

    let best_score = scored
        .iter()
        .map(|s| s.score)
        .fold(f64::NEG_INFINITY, f64::max);
    if !best_score.is_finite() || best_score < MIN_BOUNDARY_SCORE * 1.15 {
        return None;
    }

    let best = scored.iter().max_by(|a, b| {
        a.score
            .partial_cmp(&b.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            // Near ties: prefer shorter (mix earlier from end is worse than late).
            .then_with(|| b.bars.cmp(&a.bars))
    })?;
    let runner_up = scored
        .iter()
        .filter(|s| s.bars != best.bars)
        .map(|s| s.score)
        .fold(0.0f64, f64::max);
    let margin = best_score - runner_up;

    if best.bars == 8 && margin < MIN_SCORE_MARGIN * 2.0 {
        return None;
    }

    let accept = margin >= MIN_SCORE_MARGIN || best_score >= MIN_BOUNDARY_SCORE * 2.0;
    if accept {
        // Fallback path: still walk back from a credible boundary when short.
        let bars = if best.bars < FALLBACK_BARS
            && feats.len() >= best.bars as usize + OUTRO_LEAD_BARS as usize + AFTER_WIN_BARS
        {
            snap_to_bar_grid(best.bars.saturating_add(OUTRO_LEAD_BARS)).min(FALLBACK_BARS)
        } else {
            best.bars
        };
        Some(SectionEstimate {
            bars,
            low_confidence: false,
            score: best.score,
            sharpness: best.sharpness,
        })
    } else {
        None
    }
}

/// Round to the nearest multiple of 8 bars (Funkot phrase grid).
fn snap_to_bar_grid(bars: u32) -> u32 {
    if bars <= 8 {
        return 8;
    }
    let q = ((bars + 4) / 8) * 8;
    q.max(8)
}

/// Choose a snap length from per-bar features, or fall back when ambiguous.
///
/// Long cues try a 48-bar tension drop first, then {64,80,96}, then a stricter
/// mid-length {48} path (fill/shout/rise), then the earliest sustained short
/// boundary. Short {8,16,32} lengths require a sharp boundary; gradual layering
/// ramps are not accepted as short with high confidence. Mainness and candidate
/// scoring prefer the earliest sustained boundary and reject later ones that
/// already contain a clear earlier step.
fn pick_section_bars(feats: &[BarFeat]) -> SectionEstimate {
    if let Some(hit) = pick_long_intro_bars(feats) {
        return hit;
    }
    if let Some(hit) = pick_medium_intro_48(feats) {
        return hit;
    }
    if let Some(hit) = pick_by_mainness_onset(feats) {
        return hit;
    }
    if let Some(hit) = pick_by_candidate_scores(feats) {
        return hit;
    }
    SectionEstimate {
        bars: FALLBACK_BARS,
        low_confidence: true,
        score: 0.0,
        sharpness: 0.0,
    }
}

/// Detect 48/64/80/96-bar intros using cues that energy-rise scoring alone misses.
///
/// Real Funkot often keeps adding layers at 32 while the DJ intro continues, or
/// enters the main via a tension drop (quieter after the boundary). A sustained
/// drop at 48 is accepted before {64,80,96}: otherwise a mid-main fill or
/// post-drop rebuild can false-trigger 64 on a true 48-bar intro (Shirube).
/// Without 64 on this path, a post-drop midband rebuild at 80 can false-trigger
/// on true 64-bar intros (Love & Joy). Fill/shout/rise at 48 stay on the
/// stricter [`pick_medium_intro_48`] path so mid-intro layer adds do not steal
/// true 64/80/96 tracks.
fn pick_long_intro_bars(feats: &[BarFeat]) -> Option<SectionEstimate> {
    if let Some(est) = long_intro_tension_drop(feats, 48) {
        return Some(est);
    }
    for &cand in &LONG_INTRO_CANDIDATES {
        if let Some(est) = long_intro_candidate(feats, cand) {
            return Some(est);
        }
    }
    None
}

/// Sustained before→after RMS drop only (not fill/shout/rise).
fn long_intro_tension_drop(feats: &[BarFeat], cand: u32) -> Option<SectionEstimate> {
    let c = cand as usize;
    let min_before = if cand <= 48 { 24 } else { 32 };
    if c < min_before || c + 8 > feats.len() {
        return None;
    }
    if !long_intro_before_is_intro_like(feats, c) {
        return None;
    }
    let drop = median_db(&feats[c - 8..c], |f| f.rms) - median_db(&feats[c..c + 8], |f| f.rms);
    if drop < 2.0 {
        return None;
    }
    Some(SectionEstimate {
        bars: cand,
        low_confidence: false,
        score: drop,
        sharpness: drop,
    })
}

/// 48-bar intros: same family as long cues, but stricter and blocked when an
/// earlier clear main step already exists (avoids stealing 16/80/96 tracks).
fn pick_medium_intro_48(feats: &[BarFeat]) -> Option<SectionEstimate> {
    const CAND: u32 = 48;
    let c = CAND as usize;
    if c + AFTER_WIN_BARS > feats.len() {
        return None;
    }
    if before_has_clear_main_step(feats, c) {
        return None;
    }
    if !long_intro_before_is_intro_like(feats, c) {
        return None;
    }

    let after_n = AFTER_WIN_BARS.min(feats.len() - c).max(4);
    let before = &feats[..c];
    let after = &feats[c..c + after_n];
    let score = boundary_score(before, after);
    let sharpness = edge_sharpness(before, after);
    let pre = &feats[c - 4..c];
    let post = &feats[c..c + 4];
    let local_drop = median_db(pre, |f| f.rms) - median_db(post, |f| f.rms);
    let fill_lo = c.saturating_sub(10).max(c - 8);
    let fill_base = median_db(&feats[fill_lo..c - 2], |f| f.rms);
    let fill_peak = feats[c - 2..c]
        .iter()
        .map(|f| amp_to_db(f.rms.max(SILENCE_EPS)))
        .fold(f64::NEG_INFINITY, f64::max);
    let fill = fill_peak - fill_base;
    let comp = (median_f(post, |f| f.midhigh_ratio) - median_f(pre, |f| f.midhigh_ratio)).abs();
    let shout = ratio_anomaly_peak(feats, c);
    let local_rise = local_boundary_rise(pre, post);
    let local_ratio = local_ratio_step(pre, post);

    // Stricter than {80,96}: weak tension-drop / mild score must not win.
    let ok = shout >= 0.08
        || local_rise >= 3.0
        || local_ratio >= 3.0
        || (fill >= 2.0 && comp >= 0.03 && local_drop < 1.0)
        || (score >= 3.0 && sharpness.max(local_rise).max(local_ratio) >= 3.0);
    if !ok {
        return None;
    }

    Some(SectionEstimate {
        bars: CAND,
        low_confidence: false,
        score: score
            .max(fill)
            .max(local_rise)
            .max(local_ratio)
            .max(shout * 10.0),
        sharpness: sharpness.max(local_rise).max(local_ratio),
    })
}

fn long_intro_candidate(feats: &[BarFeat], cand: u32) -> Option<SectionEstimate> {
    let c = cand as usize;
    // 48 needs a slightly shorter mature window than 80/96.
    let min_before = if cand <= 48 { 24 } else { 32 };
    if c < min_before || c + AFTER_WIN_BARS > feats.len() {
        return None;
    }
    if !long_intro_before_is_intro_like(feats, c) {
        return None;
    }

    let after_n = AFTER_WIN_BARS.min(feats.len() - c).max(4);
    let before = &feats[..c];
    let after = &feats[c..c + after_n];
    let score = boundary_score(before, after);
    let sharpness = edge_sharpness(before, after);

    let before8 = &feats[c - 8..c];
    let after8 = &feats[c..c + 8];
    let pre = &feats[c - 4..c];
    let post = &feats[c..c + 4];
    let drop = median_db(before8, |f| f.rms) - median_db(after8, |f| f.rms);
    let local_drop = median_db(pre, |f| f.rms) - median_db(post, |f| f.rms);
    let fill_lo = c.saturating_sub(10).max(c - 8);
    let fill_base = median_db(&feats[fill_lo..c - 2], |f| f.rms);
    let fill_peak = feats[c - 2..c]
        .iter()
        .map(|f| amp_to_db(f.rms.max(SILENCE_EPS)))
        .fold(f64::NEG_INFINITY, f64::max);
    let fill = fill_peak - fill_base;
    let comp = (median_f(post, |f| f.midhigh_ratio) - median_f(pre, |f| f.midhigh_ratio)).abs();
    let shout = ratio_anomaly_peak(feats, c);
    // Local step without full-window variance penalty (gradual intros kill
    // [`edge_sharpness`] even when the boundary itself is crisp).
    let local_rise = local_boundary_rise(pre, post);
    let local_ratio = local_ratio_step(pre, post);

    let ok = drop >= 2.0
        || (fill >= 1.5 && local_drop < 1.0)
        || (fill >= 1.5 && comp >= 0.025 && local_drop < 1.5)
        || shout >= 0.08
        || local_rise >= 2.5
        || local_ratio >= 2.5
        || (score >= 2.0 && sharpness.max(local_rise).max(local_ratio) >= 2.5);
    if !ok {
        return None;
    }

    Some(SectionEstimate {
        bars: cand,
        low_confidence: false,
        score: score
            .max(drop)
            .max(fill)
            .max(local_rise)
            .max(local_ratio)
            .max(shout * 10.0),
        sharpness: sharpness.max(local_rise).max(local_ratio),
    })
}

/// Positive dB step across the 4 bars before/after a candidate (no variance penalty).
fn local_boundary_rise(pre: &[BarFeat], post: &[BarFeat]) -> f64 {
    let mut step = 0.0;
    step += (median_db(post, |f| f.rms) - median_db(pre, |f| f.rms)).max(0.0);
    step += (median_db(post, |f| f.hf_energy.sqrt()) - median_db(pre, |f| f.hf_energy.sqrt())).max(0.0);
    step += (median_db(post, |f| f.mid_energy.sqrt()) - median_db(pre, |f| f.mid_energy.sqrt())).max(0.0);
    step
}

/// Positive mid/high-ratio step in dB across local before/after windows.
fn local_ratio_step(pre: &[BarFeat], post: &[BarFeat]) -> f64 {
    let rb = median_f(pre, |f| f.midhigh_ratio).max(1e-8);
    let ra = median_f(post, |f| f.midhigh_ratio).max(1e-8);
    (20.0 * (ra / rb).log10()).max(0.0)
}

/// Before-window still resembles the mature intro (not already in the main body).
fn long_intro_before_is_intro_like(feats: &[BarFeat], c: usize) -> bool {
    let early_hi = 32.min(c.saturating_sub(8)).max(20);
    let early_lo = 16.min(early_hi.saturating_sub(8));
    if early_hi <= early_lo + 4 {
        return false;
    }
    let mature_lo = (c - 32).max(early_hi);
    let mature_hi = c - 8;
    if mature_hi <= mature_lo + 4 {
        // Short medium intros: compare before-8 to early intro only.
        let early = median_db(&feats[early_lo..early_hi], |f| f.rms);
        let early_mid = median_db(&feats[early_lo..early_hi], |f| f.mid_energy.sqrt());
        let before = median_db(&feats[c - 8..c], |f| f.rms);
        let before_mid = median_db(&feats[c - 8..c], |f| f.mid_energy.sqrt());
        if before < early - 1.5 {
            return false;
        }
        if before_mid > early_mid + 4.0 && before > early + 3.0 {
            return false;
        }
        return true;
    }
    let early = median_db(&feats[early_lo..early_hi], |f| f.rms);
    let early_mid = median_db(&feats[early_lo..early_hi], |f| f.mid_energy.sqrt());
    let mature = median_db(&feats[mature_lo..mature_hi], |f| f.rms);
    let before = median_db(&feats[c - 8..c], |f| f.rms);
    let before_mid = median_db(&feats[c - 8..c], |f| f.mid_energy.sqrt());
    // Still near the preceding intro level (reject post-breakdown quiet).
    if before < mature - 1.5 || before > mature + 2.0 {
        return false;
    }
    // Not already a clear main-body mid jump vs early intro.
    if before_mid > early_mid + 4.0 && before > early + 3.0 {
        return false;
    }
    true
}

fn ratio_anomaly_peak(feats: &[BarFeat], c: usize) -> f64 {
    [c.saturating_sub(1), c, c + 1]
        .into_iter()
        .filter(|&i| i > 1 && i + 1 < feats.len())
        .map(|i| {
            let mut neigh = Vec::with_capacity(4);
            for j in i.saturating_sub(2)..i {
                neigh.push(feats[j].midhigh_ratio);
            }
            for j in (i + 1)..(i + 3).min(feats.len()) {
                neigh.push(feats[j].midhigh_ratio);
            }
            if neigh.is_empty() {
                return 0.0;
            }
            neigh.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let base = neigh[neigh.len() / 2];
            feats[i].midhigh_ratio - base
        })
        .fold(0.0f64, f64::max)
}

fn pick_by_mainness_onset(feats: &[BarFeat]) -> Option<SectionEstimate> {
    if feats.len() < 4 + MAINNESS_SUSTAIN {
        return None;
    }

    let baseline = &feats[..4];
    let mainness: Vec<f64> = feats.iter().map(|f| bar_mainness(f, baseline)).collect();
    let smooth = smooth_series(&mainness, MAINNESS_SMOOTH);

    let scan = &smooth[4..];
    let peak = scan.iter().copied().fold(0.0f64, f64::max);
    if peak < MAINNESS_ENTER {
        return None;
    }
    let enter = MAINNESS_ENTER.max(peak * MAINNESS_PEAK_FRAC);
    let soft = enter * 0.75;

    // Collect every sustained onset (gradual tracks often rise early; we gate later).
    let mut onsets = Vec::new();
    let last = smooth.len().saturating_sub(MAINNESS_SUSTAIN);
    for i in 4..last {
        if smooth[i] < enter {
            continue;
        }
        if smooth[i..i + MAINNESS_SUSTAIN].iter().all(|&v| v >= soft) {
            onsets.push(i);
        }
    }
    if onsets.is_empty() {
        return None;
    }

    // Prefer the earliest snap-aligned onset that passes the boundary gate.
    // Later mid-main builds must not overwrite a true short-intro boundary.
    for &raw in &onsets {
        let (bars, snap_low) = snap_intro_bars(raw as u32);
        if snap_low {
            continue;
        }
        let c = bars as usize;
        if c + 4 > feats.len() {
            continue;
        }
        // Mid-ramp 8-bar layer adds are not mains; crisp 8-bar intros are OK.
        if bars == 8 && still_climbing_after(feats, c) {
            continue;
        }
        if before_has_clear_main_step(feats, c) {
            continue;
        }
        let after_n = AFTER_WIN_BARS.min(feats.len() - c).max(4);
        let before = &feats[..c];
        let after = &feats[c..c + after_n];
        let contrast = boundary_score(before, after);
        let sharp = edge_sharpness(before, after);
        if contrast < MIN_BOUNDARY_SCORE {
            continue;
        }
        if !passes_short_sharpness_gate(bars, before, after, sharp) {
            continue;
        }
        return Some(SectionEstimate {
            bars,
            low_confidence: false,
            score: contrast,
            sharpness: sharp,
        });
    }
    None
}

fn pick_by_candidate_scores(feats: &[BarFeat]) -> Option<SectionEstimate> {
    let mut scored: Vec<SectionEstimate> = Vec::with_capacity(INTRO_SNAP_CANDIDATES.len());
    for &cand in &INTRO_SNAP_CANDIDATES {
        let c = cand as usize;
        if c < 4 || c + 4 > feats.len() {
            continue;
        }
        if before_has_clear_main_step(feats, c) {
            continue;
        }
        let after_n = AFTER_WIN_BARS.min(feats.len() - c).max(4);
        let before = &feats[..c];
        let after = &feats[c..c + after_n];
        let score = boundary_score(before, after);
        let sharpness = edge_sharpness(before, after);
        if score < MIN_BOUNDARY_SCORE {
            continue;
        }
        if !passes_short_sharpness_gate(cand, before, after, sharpness) {
            continue;
        }
        // 8-bar hits that keep climbing afterward are mid-intro layer adds.
        // (Do not judge gradual on feats[..16] — that includes the after-window
        // and rejects true crisp 8-bar intros.)
        if cand == 8 && still_climbing_after(feats, c) {
            continue;
        }
        scored.push(SectionEstimate {
            bars: cand,
            low_confidence: false,
            score,
            sharpness,
        });
    }
    if scored.is_empty() {
        return None;
    }

    let best_score = scored
        .iter()
        .map(|s| s.score)
        .fold(f64::NEG_INFINITY, f64::max);
    if !best_score.is_finite() || best_score < MIN_BOUNDARY_SCORE * 1.15 {
        return None;
    }

    // Prefer earliest credible boundary (short intros over mid-main false hits).
    // 8-bar is only used when it is the sole survivor (else layer-adds win).
    let mut pool: Vec<SectionEstimate> = scored
        .iter()
        .copied()
        .filter(|s| s.bars > 8)
        .collect();
    if pool.is_empty() {
        pool = scored;
    }
    pool.sort_by(|a, b| {
        a.bars
            .cmp(&b.bars)
            .then_with(|| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    });

    let best = pool.first()?;
    let runner_up = pool
        .iter()
        .filter(|s| s.bars != best.bars)
        .map(|s| s.score)
        .fold(0.0f64, f64::max);
    let margin = best.score - runner_up;

    // Reject early 8-bar hits unless they dominate clearly.
    if best.bars == 8 && margin < MIN_SCORE_MARGIN * 2.0 && best.score < MIN_BOUNDARY_SCORE * 2.0 {
        return None;
    }

    let accept = margin >= MIN_SCORE_MARGIN || best.score >= MIN_BOUNDARY_SCORE * 2.0;
    if accept {
        Some(*best)
    } else {
        None
    }
}

/// True when `[0..c]` already contains an earlier snap candidate with a clear
/// sustained mainization — later candidates must not overwrite that boundary.
fn before_has_clear_main_step(feats: &[BarFeat], c: usize) -> bool {
    for &cand in &INTRO_SNAP_CANDIDATES {
        let k = cand as usize;
        if k < 8 || k + AFTER_WIN_BARS > c {
            continue;
        }
        let before = &feats[..k];
        let after = &feats[k..k + AFTER_WIN_BARS];
        let score = boundary_score(before, after);
        let sharp = edge_sharpness(before, after);
        if score < MIN_BOUNDARY_SCORE * 1.15 {
            continue;
        }
        if !passes_short_sharpness_gate(cand, before, after, sharp) {
            continue;
        }
        // Mid-intro layer adds keep climbing; they must not block later mains.
        if still_climbing_after(feats, k) {
            continue;
        }
        // Require the after-window to stay elevated vs the early intro baseline.
        if !after_stays_mainlike(feats, k) {
            continue;
        }
        return true;
    }
    false
}

fn after_stays_mainlike(feats: &[BarFeat], k: usize) -> bool {
    if k + MAINNESS_SUSTAIN > feats.len() || k < 4 {
        return false;
    }
    let baseline = &feats[..4.min(k)];
    let mainness: Vec<f64> = feats[..(k + MAINNESS_SUSTAIN).min(feats.len())]
        .iter()
        .map(|f| bar_mainness(f, baseline))
        .collect();
    let soft = MAINNESS_ENTER * 0.6;
    // Spectral sustain: midhi after boundary stays above early intro.
    let early_r = median_f(&feats[..4.min(k)], |f| f.midhigh_ratio);
    let after_r = median_f(
        &feats[k..(k + AFTER_WIN_BARS).min(feats.len())],
        |f| f.midhigh_ratio,
    );
    let spectral_ok = after_r >= early_r + 0.04
        || (early_r > 1e-8 && after_r / early_r.max(1e-8) >= 1.35);
    let energy_ok = mainness
        .get(k..k + MAINNESS_SUSTAIN)
        .map(|w| w.iter().filter(|&&v| v >= soft).count() >= MAINNESS_SUSTAIN * 2 / 3)
        .unwrap_or(false);
    spectral_ok || energy_ok
}

/// True when energy/spectrum keeps rising after `c` — mid-intro layering, not main.
fn still_climbing_after(feats: &[BarFeat], c: usize) -> bool {
    if c + 12 > feats.len() {
        return false;
    }
    let near = median_db(&feats[c..c + 4], |f| f.rms);
    let later = median_db(&feats[c + 8..c + 12], |f| f.rms);
    if (later - near) >= GRADUAL_SPREAD_DB * 0.8 {
        return true;
    }
    let near_r = median_f(&feats[c..c + 4], |f| f.midhigh_ratio);
    let later_r = median_f(&feats[c + 8..c + 12], |f| f.midhigh_ratio);
    later_r >= near_r + 0.04
        || (near_r > 1e-8 && later_r / near_r.max(1e-8) >= 1.35)
}

fn passes_short_sharpness_gate(
    bars: u32,
    before: &[BarFeat],
    after: &[BarFeat],
    sharpness: f64,
) -> bool {
    let n_b = before.len().min(4);
    let n_a = after.len().min(4);
    let (local_rise, local_ratio) = if n_b > 0 && n_a > 0 {
        let pre = &before[before.len() - n_b..];
        let post = &after[..n_a];
        (local_boundary_rise(pre, post), local_ratio_step(pre, post))
    } else {
        (0.0, 0.0)
    };
    // Energy sharpness can be zero on breakdown-into-main (RMS drops); ratio
    // and local rise still mark a crisp section change.
    let effective = sharpness.max(local_rise).max(local_ratio);

    let gradual = before_is_gradual(before);
    let min_sharp = match bars {
        8 => MIN_SHARP_8,
        16 | 32 | 48 if gradual => MIN_SHARP_SHORT_GRADUAL,
        16 | 32 | 48 => MIN_SHARP_SHORT,
        // 64+ used to free-pass; require a real gate so mid-main builds lose.
        _ if gradual => MIN_SHARP_SHORT_GRADUAL,
        _ => MIN_SHARP_SHORT,
    };
    effective >= min_sharp
}

fn before_is_gradual(before: &[BarFeat]) -> bool {
    if before.len() < 8 {
        return false;
    }
    // Monotonic layering: early quarter vs late quarter (RMS or mid/high share).
    // (Raw min-max spread is too sensitive to single fill spikes.)
    let q = before.len() / 4;
    if q == 0 {
        return false;
    }
    let early = median_db(&before[..q], |f| f.rms);
    let late = median_db(&before[before.len() - q..], |f| f.rms);
    if (late - early) >= GRADUAL_SPREAD_DB * 0.8 {
        return true;
    }
    let early_r = median_f(&before[..q], |f| f.midhigh_ratio);
    let late_r = median_f(&before[before.len() - q..], |f| f.midhigh_ratio);
    late_r >= early_r + 0.04
        || (early_r > 1e-8 && late_r / early_r.max(1e-8) >= 1.35)
}

fn bar_mainness(bar: &BarFeat, baseline: &[BarFeat]) -> f64 {
    let base_rms = median_db(baseline, |f| f.rms);
    let base_hf = median_db(baseline, |f| f.hf_energy.sqrt());
    let base_mid = median_db(baseline, |f| f.mid_energy.sqrt());
    let base_ratio = median_f(baseline, |f| f.midhigh_ratio).max(1e-8);

    let rms_d = (amp_to_db(bar.rms) - base_rms).max(0.0);
    let hf_d = (amp_to_db(bar.hf_energy.sqrt().max(SILENCE_EPS)) - base_hf).max(0.0);
    let mid_d = (amp_to_db(bar.mid_energy.sqrt().max(SILENCE_EPS)) - base_mid).max(0.0);
    let ratio_d = (20.0 * (bar.midhigh_ratio.max(1e-8) / base_ratio).log10()).max(0.0);
    1.2 * rms_d + 1.0 * mid_d + 1.0 * hf_d + 0.35 * ratio_d
}

fn smooth_series(vals: &[f64], win: usize) -> Vec<f64> {
    if vals.is_empty() {
        return Vec::new();
    }
    let w = win.max(1);
    let mut out = vec![0.0; vals.len()];
    for i in 0..vals.len() {
        let start = i.saturating_sub(w - 1);
        let slice = &vals[start..=i];
        out[i] = slice.iter().sum::<f64>() / slice.len() as f64;
    }
    out
}

fn snap_bars(raw: u32) -> (u32, bool) {
    snap_to_candidates(raw, &SNAP_CANDIDATES)
}

fn snap_intro_bars(raw: u32) -> (u32, bool) {
    snap_to_candidates(raw, &INTRO_SNAP_CANDIDATES)
}

fn snap_to_candidates(raw: u32, candidates: &[u32]) -> (u32, bool) {
    let mut best = candidates[0];
    let mut best_dist = u32::MAX;
    for &c in candidates {
        let d = raw.abs_diff(c);
        if d < best_dist {
            best_dist = d;
            best = c;
        }
    }
    let rel = f64::from(best_dist) / f64::from(best.max(1));
    // Absolute slack covers near-misses like raw=21 → 16 (rel 0.31 > 0.25).
    if rel > SNAP_TOLERANCE && best_dist > SNAP_ABS_TOLERANCE {
        (FALLBACK_BARS, true)
    } else {
        (best, false)
    }
}

/// Contrast of a candidate intro/outro window vs the following main-body sample.
///
/// Real Funkot often adds bass/mid layers at the main without a 4× HF-ratio jump
/// (ratio can even fall as low energy rises). Score positive dB-like deltas in
/// RMS, mid-band energy, absolute HF energy, and HF ratio.
fn boundary_score(before: &[BarFeat], after: &[BarFeat]) -> f64 {
    if before.is_empty() || after.is_empty() {
        return 0.0;
    }
    let rms_d = (median_db(after, |f| f.rms) - median_db(before, |f| f.rms)).max(0.0);
    let mid_d = (median_db(after, |f| f.mid_energy.sqrt())
        - median_db(before, |f| f.mid_energy.sqrt()))
    .max(0.0);
    let hf_d = (median_db(after, |f| f.hf_energy.sqrt())
        - median_db(before, |f| f.hf_energy.sqrt()))
    .max(0.0);
    let ratio_b = median_f(before, |f| f.midhigh_ratio).max(1e-8);
    let ratio_a = median_f(after, |f| f.midhigh_ratio).max(1e-8);
    let ratio_d = (20.0 * (ratio_a / ratio_b).log10()).max(0.0);

    // Local sharpness: step at the boundary vs variation inside each side.
    let edge = edge_sharpness(before, after);

    1.2 * rms_d + 1.0 * mid_d + 1.0 * hf_d + 0.35 * ratio_d + 0.5 * edge
}

fn edge_sharpness(before: &[BarFeat], after: &[BarFeat]) -> f64 {
    let n_b = before.len().min(4);
    let n_a = after.len().min(4);
    if n_b == 0 || n_a == 0 {
        return 0.0;
    }
    let pre = &before[before.len() - n_b..];
    let post = &after[..n_a];
    let step = (median_db(post, |f| f.rms) - median_db(pre, |f| f.rms)).max(0.0)
        + (median_db(post, |f| f.hf_energy.sqrt()) - median_db(pre, |f| f.hf_energy.sqrt()))
            .max(0.0)
        + (median_db(post, |f| f.mid_energy.sqrt()) - median_db(pre, |f| f.mid_energy.sqrt()))
            .max(0.0);
    // Penalize if the before/after windows themselves already swing a lot.
    let var_b = window_spread_db(before);
    let var_a = window_spread_db(after);
    (step - 0.55 * (var_b + var_a)).max(0.0)
}

fn window_spread_db(feats: &[BarFeat]) -> f64 {
    if feats.len() < 2 {
        return 0.0;
    }
    let mut vals: Vec<f64> = feats
        .iter()
        .map(|f| amp_to_db(f.rms.max(SILENCE_EPS)))
        .collect();
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    vals[vals.len() - 1] - vals[0]
}

fn median_f(feats: &[BarFeat], key: impl Fn(&BarFeat) -> f64) -> f64 {
    let mut vals: Vec<f64> = feats.iter().map(key).collect();
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    vals[vals.len() / 2]
}

fn median_db(feats: &[BarFeat], key: impl Fn(&BarFeat) -> f64) -> f64 {
    amp_to_db(median_f(feats, key).max(SILENCE_EPS))
}

fn amp_to_db(amp: f64) -> f64 {
    20.0 * amp.max(SILENCE_EPS).log10()
}

/// Per-bar diagnostics for head/tail section detection (used by `section_diag` example).
#[derive(Debug, Clone)]
pub struct BarDiag {
    pub bar_index: usize,
    pub midhigh_ratio: f64,
    pub rms_db: f64,
    pub hf_db: f64,
    pub centroid_hz: f64,
}

/// Aggregated head/tail bar features for offline inspection.
#[derive(Debug, Clone)]
pub struct SectionDiag {
    pub intro: Vec<BarDiag>,
    pub outro: Vec<BarDiag>,
}

/// Compute per-bar features for intro (forward) and outro (backward) scan windows.
pub fn diagnose_section_bars(buffer: &AudioBuffer) -> Result<SectionDiag> {
    let sr = buffer.sample_rate as f64;
    let segment_frames = (SEGMENT_SECS * sr).round() as u64;
    let head_len = segment_frames.min(buffer.frames);
    let head = buffer.mono_range(0, head_len);
    let head_onset = onset_envelope(&head, buffer.sample_rate, HOP)?;
    let intro_bpm = estimate_bpm(&head_onset.novelty, HOP, buffer.sample_rate)?;
    let head_beat_period_hops = bpm_to_period_hops(intro_bpm, HOP as f64, sr);
    let first_downbeat_hop = find_first_downbeat_hop(
        &head_onset.novelty,
        &head_onset.energy,
        head_beat_period_hops,
        HOP,
    )?;
    let first_downbeat = refine_kick_marker_mono(
        &head,
        buffer.sample_rate,
        first_downbeat_hop,
        KICK_REFINE_RADIUS_MS,
    );
    let intro_bar_len = (60.0 / intro_bpm * sr * f64::from(BEATS_PER_BAR)).max(1.0);

    let tail_start = buffer.frames.saturating_sub(segment_frames);
    let tail = buffer.mono_range(tail_start, buffer.frames - tail_start);
    let tail_onset = onset_envelope(&tail, buffer.sample_rate, HOP)?;
    let outro_bpm = estimate_bpm(&tail_onset.novelty, HOP, buffer.sample_rate)?;
    let outro_bar_len = (60.0 / outro_bpm * sr * f64::from(BEATS_PER_BAR))
        .round()
        .max(1.0);

    Ok(SectionDiag {
        intro: bar_diag_rows(buffer, first_downbeat, intro_bar_len, SectionDir::Forward)?,
        outro: bar_diag_rows(buffer, buffer.frames, outro_bar_len, SectionDir::Backward)?,
    })
}

fn bar_diag_rows(
    buffer: &AudioBuffer,
    anchor: u64,
    bar_len: f64,
    dir: SectionDir,
) -> Result<Vec<BarDiag>> {
    let bar_len_frames = bar_len.round().max(1.0) as u64;
    let max_scan = match dir {
        SectionDir::Forward => MAX_SCAN_BARS_INTRO,
        SectionDir::Backward => MAX_SCAN_BARS_OUTRO,
    };
    let n_bars = match dir {
        SectionDir::Forward => {
            max_scan.min(((buffer.frames.saturating_sub(anchor)) / bar_len_frames) as usize)
        }
        SectionDir::Backward => max_scan.min((anchor / bar_len_frames) as usize),
    };
    let starts: Vec<u64> = (0..n_bars)
        .map(|i| match dir {
            SectionDir::Forward => anchor + i as u64 * bar_len_frames,
            SectionDir::Backward => anchor.saturating_sub((i as u64 + 1) * bar_len_frames),
        })
        .collect();
    let feats = bar_features(buffer, &starts, bar_len_frames);
    Ok(feats
        .into_iter()
        .enumerate()
        .map(|(i, f)| BarDiag {
            bar_index: i,
            midhigh_ratio: f.midhigh_ratio,
            rms_db: if f.rms > SILENCE_EPS {
                20.0 * f.rms.log10()
            } else {
                -120.0
            },
            hf_db: if f.hf_energy > SILENCE_EPS {
                20.0 * f.hf_energy.sqrt().log10()
            } else {
                -120.0
            },
            centroid_hz: f.centroid_hz,
        })
        .collect())
}

#[derive(Clone, Copy)]
struct BarFeat {
    midhigh_ratio: f64,
    rms: f64,
    /// Mean-square energy above [`HIGHPASS_HZ`].
    hf_energy: f64,
    /// Mean-square energy below the high-pass (kick / bass / mid body).
    mid_energy: f64,
    centroid_hz: f64,
}

fn bar_features(buffer: &AudioBuffer, bar_starts: &[u64], bar_len_frames: u64) -> Vec<BarFeat> {
    let sr = f64::from(buffer.sample_rate);
    let mut out = Vec::with_capacity(bar_starts.len());
    for &start in bar_starts {
        let len = bar_len_frames.min(buffer.frames.saturating_sub(start));
        if len == 0 {
            out.push(BarFeat {
                midhigh_ratio: 0.0,
                rms: 0.0,
                hf_energy: 0.0,
                mid_energy: 0.0,
                centroid_hz: 0.0,
            });
            continue;
        }
        let mono = buffer.mono_range(start, len);
        let high = highpass_1pole(&mono, sr, HIGHPASS_HZ);
        let mut total = 0.0f64;
        let mut hi = 0.0f64;
        // Cheap spectral centroid proxy via zero-crossing rate + HF share.
        let mut zc = 0u32;
        let mut prev = 0.0f32;
        for (&t, &h) in mono.iter().zip(high.iter()) {
            let tf = f64::from(t);
            let hf = f64::from(h);
            total += tf * tf;
            hi += hf * hf;
            if (t >= 0.0) != (prev >= 0.0) {
                zc += 1;
            }
            prev = t;
        }
        let n = mono.len().max(1) as f64;
        let rms = (total / n).sqrt();
        let hf_energy = hi / n;
        let mid_energy = ((total - hi).max(0.0)) / n;
        let ratio = if total > SILENCE_EPS { hi / total } else { 0.0 };
        let zcr_hz = (f64::from(zc) * sr) / (2.0 * n);
        let centroid_hz = 0.35 * zcr_hz + 0.65 * (HIGHPASS_HZ * ratio.max(1e-6).sqrt() * 4.0);
        out.push(BarFeat {
            midhigh_ratio: ratio,
            rms,
            hf_energy,
            mid_energy,
            centroid_hz,
        });
    }
    out
}

/// Full-track mono RMS → dBFS and gain toward [`TARGET_RMS_DBFS`], clamped to ±12 dB.
fn measure_loudness(buffer: &AudioBuffer) -> (f64, f64) {
    let mono = buffer.mono();
    if mono.is_empty() {
        return (TARGET_RMS_DBFS, 0.0);
    }
    let mut sum_sq = 0.0f64;
    for &s in &mono {
        let x = f64::from(s);
        sum_sq += x * x;
    }
    let rms = (sum_sq / mono.len() as f64).sqrt();
    if rms < SILENCE_EPS {
        return (-120.0, 0.0);
    }
    let rms_dbfs = 20.0 * rms.log10();
    let gain_db = (TARGET_RMS_DBFS - rms_dbfs).clamp(-GAIN_CLAMP_DB, GAIN_CLAMP_DB);
    (rms_dbfs, gain_db)
}
