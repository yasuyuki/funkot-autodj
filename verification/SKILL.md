---
name: verify-funkot-transition-geometry
description: Verify Funkot AutoDJ transition geometry, deck admission and asynchronous manual navigation after changes to planning, markers, navigation, source fencing or their Kani and TLC checks.
---
# Verify transition geometry

Use an explicitly selected `funkot-autodj` working-set member. Read its
`AGENTS.md` and `verification/README.md`. The shared editing source is
`agent-rules/skills/verify-funkot-transition-geometry/SKILL.md`; the core's
`verification/SKILL.md` is a byte-identical distribution, reached from its
project instructions. Update the source and distribution together.

Last synchronization with product code is
`ff68fe49e5f96ab4d4bb528d25ef44cc38752bb8`. This is an audit anchor, not a pin.
Advance it only after reading every intervening change to covered code and
confirming that the verifier still describes the new source. A pass against
stale caller assumptions is not a product proof.

## Covered promises

`funkot-core/src/transition_geometry.rs` is both production and proof source.
`plan_transition` keeps its schedule within the intro/outro budgets for all
independent `u32` fade, intro and outro inputs, including zero and maximum values.
`outro_trigger_bars` keeps its trigger in the track body after the intro. Only
a structural boundary within that body gets an ordering and lead guarantee.
The full 16-bar lead is required whenever it physically fits. Contradictory
boundaries clip to the intro end. Counts are measured from the track end.
`admit_frames` checks arbitrary `u64` lengths and positions. An accepted full
plan keeps its endpoints and continuation horizon within both decks. A simple
plan clips its fade to common remaining material.

The adapter coverage is `engine.rs::bar_to_frames`, `build_manual_plan`,
`commit_manual_plan`, `begin_transition_to`, `begin_transition`, `start_first`,
`finish_prepare`, `Engine::new_with_source`, `Engine::from_prepared`, and
`Engine::from_prepared_first_live`. Inspect every new path that moves
their decks, entries, nudges or endpoints. The full target horizon comes from
the absolute main marker and corrected entry, including a real frame at main.
Manual rejection precedes queue/history consumption and start events.
Automatic fallback discards a rejected correction, then uses a bounded simple
fade or hard cut. Cached derived triggers are normalized by `cache.rs::read_entry`
without changing manual input fields or stored bytes.
These constructors reject non-finite bar durations and durations below
one output frame before starting workers or adopting tracks.

## Run and judge

From the selected core root on Linux x86_64 with Docker and cgroup v2:

```sh
./scripts/check-transition-geometry.sh --setup
./scripts/check-transition-geometry.sh
./dev.sh cargo test -p funkot-core --release --lib
./dev.sh cargo test -p funkot-core --release --test engine --test fade_curve
./work.sh --self-check
```

The proof entry fixes Kani 0.68.0, bundled Rust and CBMC. Its result validator
requires all named harnesses and all covers. No proof uses `kani::assume`;
there are no loops or recursion needing an input-size or unwind bound. Inspect
`summary.json`, `kani.json` and `kani.log`. Timeout, missing/failed checks,
unsupported reachability and unreached covers are failures, not a reduced proof.
The existing 15-minute deadline and 8-GiB cgroup limit bound the verifier process
tree. Keep the per-run revision, source hashes, elapsed time and peak memory.

A failing pure-function assertion is a product bug when its counterexample can
occur in the shared source. A wrong mapping from caller state or a stale absolute
main/horizon assumption is verification drift. Fix that mapping from the code,
then rerun before classifying the product. Preserve the property when fixing a
bug; change it only for an explicitly changed promise and record that evidence.

## Demonstrate detection

Use isolated copies of the actual production source and driver. Never apply
mutations to the product branch. The ordinary entry must pass again afterward.

- Replace `let skip = i.saturating_sub(m)` with `let skip = i`.
  `plan_all_u32` must fail skip-plus-intro and/or overflow checks.
- Replace the trigger's `saturating_add(OUTRO_LEAD_BARS)` with
  `saturating_add(0)`. `trigger_all_u32` must fail the lead requirement.
- Remove `fade_out_end > prev_frames - prev_start` from full frame admission.
  `frame_admission_all_u64` and
  `engine::tests::invalid_manual_correction_keeps_decks_queues_and_events_unchanged`
  must fail. A test that still passes after this break is not acceptance evidence.

The real-engine regressions also distinguish an absolute main at or before PCM
end, reject empty or mismatched prepared PCM, retain exact admitted entry/start,
keep the last one-frame simple fade audible, and reject unrepresentable bar
durations at both engine constructors. Preserve those observable
boundaries when updating the checker after a code change.

## Manual navigation ordering

The navigation coverage is `engine.rs::begin_nav`, `poll_nav_tempo`,
`manual_plan_matches`, `kick_manual_plan_if_idle`, `tick_pending_nav`,
`commit_manual_plan`, `cancel_pending_nav`, `drop_prev`, `push_history`,
`apply_upgrade`, `drain_loader`, and the begin/abort/commit future-update methods.
Inspect every new writer of their active/target/history identities, navigation
generation, deadline or future fence. A source change invalidates navigation
through generation; `ManualKey` has no separate source-epoch field.

`funkot-core/src/manual_navigation.rs` is the production decision source.
`verification/manual_navigation.rs` directly imports it. The existing Kani
entry also checks both navigation harnesses and their reachable covers over
arbitrary `u64` frames/generations and optional facts. Preserve exact-start
adoption, exact-at-deadline precedence, a fixed replan deadline, and current
fallback construction. Borrowed identity facts remain an Engine adapter and
must be checked by real Engine regressions.

Run the finite event-order check on Linux with Python 3.12 or later and Java 21:

```sh
python3 -B verification/check_manual_navigation.py --setup
python3 -B verification/check_manual_navigation.py
```

Use the normal clean-checkout entries. They verify a commit archive and refuse
unavailable/malformed revisions, dirty source and nonempty evidence directories.
Read both `summary.json` results and the CI artifacts. Missing results, partial
exploration, unexpected tool bytes, timeout and an undetected mutation fail.
The navigation runner fixes TLC 1.7.4 by hash. `verification/README.md` owns the
bounds, fairness assumptions, transition-to-code table and unproved scope.

Read each TLC result with `/statechart`. Replay a counterexample in real Rust
before calling it a product bug. The accepted-Prev/history-rollover trace is
already replayed by `history_rollover_cancels_an_accepted_previous_navigation`.
Preserve its cancellation and the companion Next/Restart preservation test.

The runner checks generation, exact-start, missing-deadline and old-history
variants in isolated model copies. The shared Rust generation/identity/deadline/
start variants must fail completed Kani safety assertions and their named Engine
regressions. `manual_navigation_mutations.py` owns edits and test names; the
native identity variant also disables the independent Engine identity recheck.
A compiler error, a missing test or a timeout is not mutation detection.
The native suite must pass again on the unchanged committed source afterward.

A model mismatch is verification drift. An actual accepted target silently
changing during a pending operation is a product bug. Keep the promise while
repairing either. The stable TLC configuration proves adoption only under its
stated noninterference and progress assumptions. The ordinary configuration checks safety over the complete bounded event
graph; conditional resolution and adoption are checked by the stable configuration.
Neither model covers sound quality, devices, arbitrary-length operation streams,
or the complete first-live preview/full lifecycle.

## Limits and result record

The numeric proof does not verify floating-point conversion or that its caller
passes the correct musical marker. Those adapters are covered by engine
regressions. First-live readiness, upgrade adoption, preparation identities and
preview wait require real-engine regressions, outside these integer proofs.
Phase scores, musical bar identity, click freedom, real music,
devices, realtime schedules, OS behavior and the #22 candidate-selection
question remain outside the proof. Run normal workspace checks and real-track
rendering as required by `AGENTS.md`; report unavailable input explicitly.

Record geometry results in `funkot-autodj` Issue #26 and navigation results in
Issue #41, including counterexamples, mutations, exact source/CI commits and
unproved scope. Keep private paths, raw diagnostics
and per-file hashes in private evidence; link public CI artifacts for shared
logs. After changes that skipped this check, use
`maintain-formal-verification-skill` to classify each result as in sync, updated,
bug or blocked. Verification updates belong in a separate commit from product
fixes.
