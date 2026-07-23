//! funkot-core: Funkot auto-DJ mixing engine.
//!
//! Design summary (see README for the full spec):
//! - Funkot tracks are nominally 180 BPM; intros/outros are steady machine
//!   rhythm, mid-song tempo is unreliable. Only the head/tail are analyzed.
//! - Every track is time-stretched by a single constant ratio
//!   `target_bpm / track_intro_bpm` where `target_bpm = 180 * rate`
//!   (rate defaults to 1.10 → 198 BPM). Pitch is preserved by default.
//! - Transition is scheduled backward from the next track's intro end (T0):
//!   next enters `2·fade_bars + MAIN_GAP_BARS` earlier (HPF, vol 0), fades in
//!   linearly over `fade_bars`, then HPF flips to the previous track and that
//!   track fades out linearly so it reaches silence exactly [`MAIN_GAP_BARS`]
//!   (8) bars before T0; the previous deck is dropped at that endpoint.
//!   Long intros are entered mid-way (`skip`) so the audible overlap stays the
//!   fade pair only (~8 bars at default F=4).

pub mod analysis;
pub mod cache;
pub mod decode;
pub mod engine;
pub mod ffi;
pub mod filter;
pub mod stretch;

#[cfg(any(test, feature = "testutil"))]
pub mod testutil;

#[cfg(test)]
mod analysis_tests;

use serde::{Deserialize, Serialize};

/// Nominal Funkot tempo. The stretch target is `NOMINAL_BPM * options.rate`.
pub const NOMINAL_BPM: f64 = 180.0;

/// Beats per bar (Funkot is straight 4/4).
pub const BEATS_PER_BAR: u32 = 4;

/// Bars of the next track's intro that remain after the previous track's
/// fade-out has completed (and before the next main section starts).
pub const MAIN_GAP_BARS: u32 = 8;

/// Obsolete: the compact schedule always leaves exactly [`MAIN_GAP_BARS`] of
/// next-intro solo; long intros are entered via `skip` instead of a longer
/// solo cap. Kept so older callers still compile.
#[deprecated(note = "solo intro is always MAIN_GAP_BARS; unused by plan_transition")]
pub const MAX_SOLO_INTRO_BARS: u32 = 16;

/// Fallback intro/outro length when automatic estimation is not confident.
/// Prefer the longest plausible Funkot section over a short false positive.
pub const FALLBACK_BARS: u32 = 64;

/// Reference loudness for RMS gain normalization, in dBFS.
pub const TARGET_RMS_DBFS: f64 = -14.0;

/// Result of the one-time per-track analysis. Serialized as JSON into the
/// cache directory; users may hand-edit any field (e.g. `intro_bars`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TrackAnalysis {
    /// Cache format version; bump when the analyzer changes incompatibly.
    pub version: u32,
    /// Original file name (informational, for hand-editing convenience).
    pub file_name: String,
    /// Sample rate of the decoded audio this analysis refers to.
    pub sample_rate: u32,
    /// Total length in frames (per-channel samples) of the decoded track.
    pub total_frames: u64,
    /// BPM measured over the intro section.
    pub intro_bpm: f64,
    /// BPM measured over the outro section.
    pub outro_bpm: f64,
    /// Frame index of the first downbeat (bar boundary) of the intro.
    pub first_downbeat: u64,
    /// Frame index of the downbeat on which the outro starts.
    pub outro_start: u64,
    /// Intro length in bars.
    pub intro_bars: u32,
    /// Outro mix-trigger length in bars (from file end). Full energy-drop
    /// boundary plus ~16 bars of lead-in so DJ mixing starts before collapse.
    pub outro_bars: u32,
    /// `true` when either intro or outro bar count is low-confidence
    /// (compat aggregate of the per-side flags).
    pub bars_estimated_low_confidence: bool,
    /// `true` when `intro_bars` came from the fallback / ambiguous estimate.
    #[serde(default)]
    pub intro_bars_low_confidence: bool,
    /// `true` when `outro_bars` came from the fallback / ambiguous estimate.
    #[serde(default)]
    pub outro_bars_low_confidence: bool,
    /// Measured RMS loudness of the whole analyzed material, in dBFS.
    pub rms_dbfs: f64,
    /// Gain in dB to reach [`TARGET_RMS_DBFS`]. Applied unless disabled.
    pub gain_db: f64,
}

/// How playback speed change affects pitch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PitchMode {
    /// Time-stretch: tempo changes, pitch preserved (default).
    #[default]
    Preserve,
    /// Plain resample: pitch rises with tempo, like a turntable.
    Shift,
}

/// Engine configuration shared by CLI and FFI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineOptions {
    /// Speed-up factor applied to the nominal 180 BPM. Default 1.10 → 198.
    pub rate: f64,
    /// Pitch behaviour of the speed change.
    pub pitch_mode: PitchMode,
    /// Fade-in and fade-out length in bars (spec: 4–8, default 4). Shrunk
    /// automatically when a short intro cannot fit the full schedule.
    pub fade_bars: u32,
    /// High-pass cutoff (Hz) for the mid/high-pass during transitions
    /// (Butterworth ~300 Hz: pass above cutoff, attenuate bass).
    pub highpass_hz: f32,
    /// Apply per-track RMS gain normalization.
    pub gain_normalize: bool,
    /// Shuffle playlist order (reshuffled every full cycle).
    pub random: bool,
    /// Repeat the playlist forever. When `false`, playback stops after one
    /// pass, finishing at the end of the last track's outro.
    pub loop_playlist: bool,
    /// Output sample rate in Hz.
    pub output_sample_rate: u32,
    /// Directory for analysis cache JSON files.
    pub cache_dir: std::path::PathBuf,
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self {
            rate: 1.10,
            pitch_mode: PitchMode::Preserve,
            fade_bars: 4,
            highpass_hz: 300.0,
            gain_normalize: true,
            random: false,
            loop_playlist: true,
            output_sample_rate: 48_000,
            cache_dir: std::path::PathBuf::from("funkot-cache"),
        }
    }
}

impl EngineOptions {
    /// The tempo every track is stretched to, in BPM.
    pub fn target_bpm(&self) -> f64 {
        NOMINAL_BPM * self.rate
    }

    /// Duration of one bar at the target tempo, in output frames.
    pub fn bar_frames(&self) -> f64 {
        self.output_sample_rate as f64 * 60.0 / self.target_bpm() * BEATS_PER_BAR as f64
    }
}

/// Errors produced by funkot-core.
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Decode(String),
    UnsupportedFormat(String),
    Analysis(String),
    Cache(String),
    /// Offline time-stretch / resample failure.
    Stretch(String),
    /// Engine construction or internal failure.
    Engine(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Decode(m) => write!(f, "decode error: {m}"),
            Error::UnsupportedFormat(m) => write!(f, "unsupported format: {m}"),
            Error::Analysis(m) => write!(f, "analysis error: {m}"),
            Error::Cache(m) => write!(f, "cache error: {m}"),
            Error::Stretch(m) => write!(f, "stretch error: {m}"),
            Error::Engine(m) => write!(f, "engine error: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
