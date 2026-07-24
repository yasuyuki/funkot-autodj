# Changelog

All notable changes to this project are documented in this file.

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
