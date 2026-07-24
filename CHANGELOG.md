# Changelog

All notable changes to this project are documented in this file.

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
