# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

### Fixed

- `TrackAnalysis::is_funkot` no longer rejects most Funkot. The previous rule re-ran the BPM argmax over a wide 100–200 band and required both intro and outro to land in 172..=188; measured over 69 known-Funkot tracks it passed only 23. The comb score it compared is a *mean* over `len / period` samples, so longer periods ride higher on noise and the argmax went to metrical-level aliases of 180 — observed landing sites were 135 (3/4), 120 (2/3) and 112.5 (5/8). The score is now normalised for its sample count, the grid period is judged against the best score in the band instead of by argmax, a half-tempo veto is applied, and one dead side no longer sinks a track. Over the same corpora: 69/69 known Funkot (was 23/69), 60/63 on a second known-Funkot set (was 24/63), and 20/261 false positives on hand-labeled non-Funkot (was 15/261). `CACHE_VERSION` 12 → 13, so existing caches reanalyze.
- Outro floor sampling falls back to the lower quartile when a bright final fill spikes the near-end window (max ≫ median), so true 32-bar outros are no longer reported as 48 (Starmine). Purge auto-cache to refresh older analyses.
- `--label-sections` guide clicks land on the music's beat. Outro candidates are counted back from `total_frames`, whose phase is whatever the master's last sample happened to be — measured over the real test set it is spread across the full ±0.5 beat, and on IVY it is +0.494 beat, i.e. every outro click was on the off-beat. Both sides' click grids are now phase-locked to the listening window itself (`analysis::lock_beat_phase`). Worst measured click-vs-music error over the sample tracks drops from 0.45 beat to 0.10 beat. Bar identity is unchanged: the lock never moves a boundary by half a beat or more.

### Added

- `analysis::lock_beat_phase`: full-beat-period phase lock from a broadband spectral-flux onset comb, for markers whose phase is unknown a priori. Complements `refine_groove_phase`, which micro-aligns (±0.45 beat, low-band) markers that are already approximately right.
- `funkot-cli` example `click_phase_diag`: prints, per track and candidate, the nominal vs. locked `--label-sections` boundary and the shift in beats. Headless (no audio device).
- `funkot-core` example `classify_probe`: prints, per track, the three quantities `is_funkot` decides on and the verdict. Run it over the labeled playlists in `testdata/` before and after touching the classifier — the thresholds were chosen from that table and a change that helps one corpus usually costs another. Headless.

- Engine `TrackSource` trait and `Engine::new_with_source`: the loader asks the host for each track instead of owning a fixed `Vec<PathBuf>`, so a host-owned queue can be appended to, reordered or trimmed without restarting playback. `Engine::new` is unchanged and now wraps the same default shuffle/loop behaviour.
- `./cross-build.sh android` cross-builds `funkot-core` for `aarch64-linux-android` and packages a C-ABI SDK (`libfunkot_core.{so,a}`, `libc++_shared.so`, `include/funkot.h`) into `dist/android-arm64/`. Guards against dependency changes that break the NDK build.
- `cache::set_manual_bars` hand-edits `intro_bars` / `outro_bars` on a cached entry and persists it, recomputing `outro_start` so callers no longer have to. The side passed as `None` is untouched, including its `*_manual` flag, and `needs_reanalysis` is preserved.

## [0.3.1] - 2026-07-25

### Fixed

- Live playback under CPU load: automatic transitions no longer run kick/hat phase-align inside the audio callback when the worker result is late (use nominal entry instead), and large prepared buffers (Upgrade, surplus Ready, rewind history) are freed off the audio thread to avoid underrun clicks. Manual skip/rewind still uses its existing align path.
- Intro detection accepts a brightness tension drop (mid/high share falls while RMS holds) at 48 bars, so vocal mains whose hats step back are no longer overrun by a later fill at 64 (IVY). Auto-cached analyses from before this change keep the old length until `--purge-auto-cache`.
- Intro detection checks the stricter 48-bar fill/shout/rise cue before {64,80,96} long cues, so pre-main spectral shouts (Starmine) are not overwritten by a quieter mid-main at 64. Purge auto-cache to refresh older analyses.

## [0.3.0] - 2026-07-25

### Added

- Live skip/rewind navigation via left/right arrow multi-tap (≤500ms window): restart current, previous/next at normal entry, or immediate jump to previous/next intro.
- Local tempo analysis around the playhead; source-equivalent 172–188 BPM uses the usual DJ transition (bar wait, HPF, phase lock), otherwise a simple linear crossfade without HPF/phase-align.
- Crossterm raw-mode keyboard handling in the CLI (Enter pause/resume; fallback to line-mode Enter if raw mode is unavailable).
- Engine `NavAction` / `nav_sender` API and rewind history of the previous deck for reverse navigation.

## [0.2.1] - 2026-07-24

### Fixed

- Live transitions no longer glitch under load: phase-align runs on a worker thread, and finished decks are dropped off the audio thread.
- First-live mixes on a cold cache no longer fire early from provisional FALLBACK outro (64 bars) while analysis already reported the real length; Upgrade waits for real outro markers.
- Intro detection prefers a sustained RMS tension drop at 48 (then 64+) over later mid-main fill/rebuild cues (false 80 / false 64 cases).

### Added

- Hand-edited `intro_bars` / `outro_bars` are preserved across `--purge-auto-cache` and reanalysis via `intro_bars_manual` / `outro_bars_manual` and `needs_reanalysis`.
- `--transitions-only` plays (or with `--render`, writes) the same transition windows used for per-transition clip export.
- CLI prints `intro_bars` / `outro_bars` / `bpm` when analysis is ready on `TrackStarted`.

## [0.2.0] - 2026-07-24

### Breaking

- Analysis cache formats v4–v7 are invalidated (`CACHE_VERSION` is now 8). Old cache JSON is ignored and tracks are re-analyzed. Re-apply any manual `intro_bars` / `outro_bars` edits after upgrading.
- Transition scheduling is based on the next track’s intro end (T0) with a compact fade pair. Audible overlap is shorter than in v0.1.0; mix length and cut points change for the same playlist.

### Changed

- Intro detection: candidates extended to 8–96 bars; confident `intro < outro` pairs are kept; short/long intros no longer collapse incorrectly to 64.
- Outro detection: full mid-band energy drop plus 16-bar lead (typically ~end−48); mid-outro plateaus are rejected.
- `--transition-clip-seconds` default is 60s; clips start 8 bars before each transition.

### Added

- Bahasa Indonesia README (`README_id.md`), linked from English and Japanese docs.

## [0.1.0] - 2026-07-23

Initial public release: CLI and C ABI SDK builds for linux-x64, windows-x64, and macos-arm64.
