# Transition geometry

The driver imports `funkot-core/src/transition_geometry.rs` directly. The engine
and analysis modules call that same source. Kani dependencies stay outside the
release Cargo workspace, Android build and C ABI.

From the core repository root on Linux x86_64 with Docker and cgroup v2:

```sh
./scripts/check-transition-geometry.sh --setup
./scripts/check-transition-geometry.sh /tmp/transition-proof
```

The fixed verifier is Kani 0.68.0, bundled Rust nightly-2026-08-21 and CBMC
6.11.0. The runner reuses the player verification entry's 15-minute deadline
and 8-GiB cgroup limit without extra swap. `summary.json` retains elapsed time,
peak memory, revision and source hashes. `kani.json` retains individual checks.
The runner requires a readable Git checkout with a valid HEAD commit and no
tracked or nonignored untracked changes. This applies to clones and linked
worktrees. Keep evidence outside the checkout or in an ignored directory.
It streams `git archive` of that commit into the verifier. Git metadata is not
needed inside Docker, and later checkout edits cannot change the proof source.
An unavailable revision, invalid revision or dirty checkout writes a FAIL summary
and exits before Docker. The validator also rejects missing or malformed revisions.
Timeout, solver failure, missing harnesses and unreached covers fail the command.
Default overflow, assertion, pointer and unwinding checks stay enabled. Neither
function has a loop or recursion, so there is no finite input enumeration or
loop-unwind assumption. The bar proofs quantify independent, unrestricted `u32`
inputs without `kani::assume`. Frame admission quantifies arbitrary `u64`
positions and lengths, plus the simple/full mode.

`plan_all_u32` proves that the compact schedule and fades fit their intro and
outro budgets, that skip plus remaining intro equals the intro, and that the
solo gap remains when the intro can contain it. Zero effective fade is valid
when no fade pair fits. `trigger_all_u32` proves a trigger within the track
body after the intro. For a structural boundary within that body, the trigger
leads it by up to 16 bars, with the full lead whenever room exists. Otherwise
the trigger clips to the intro end. Counts are from the end of the track, so
a larger trigger count means an earlier playback position.

`frame_admission_all_u64` proves that accepted frame ranges fit both decks,
retain the checked previous start and target entry, and retain the requested
endpoints for a full plan. A simple plan clips its fade to common remaining
material. A full plan also requires its target continuation horizon to fit.

The proofs cover integer bar geometry. Floating-point bar-to-frame conversion,
phase correction and deck installation are checked by real engine regressions.
They do not prove phase scores, musical bar identity, real-track sound quality,
click freedom, real-time scheduling, devices, or OS behavior. The 8-bar candidate
question in [#22](https://github.com/yasuyuki/funkot-autodj/issues/22) is separate
and is not an input assumption.

The runner repeats the three sensitivity checks below in isolated temporary
trees within the same total deadline. Each must produce a completed Kani run
with a failing safety assertion. Compilation errors and timeouts do not qualify.
For a manual sensitivity check, copy the production module and driver into an isolated
tree. Replacing `let skip = i.saturating_sub(m)` with `let skip = i` must fail
`plan_all_u32`. Replacing the trigger's `saturating_add(OUTRO_LEAD_BARS)` with
`saturating_add(0)` must fail `trigger_all_u32`. Run the unchanged source again
after discarding the isolated variants. Keep real-code regressions for the
adoption boundary, since integer proofs cannot detect an omitted admission check.
Removing the full-admission previous-material bound must fail both
`frame_admission_all_u64` and the real-engine
`invalid_manual_correction_keeps_decks_queues_and_events_unchanged` regression.

Results and unproved scope are recorded in
[#26](https://github.com/yasuyuki/funkot-autodj/issues/26).
