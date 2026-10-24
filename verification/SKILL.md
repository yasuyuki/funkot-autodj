---
name: verify-funkot-transition-geometry
description: Verify Funkot AutoDJ transition bar geometry and real deck admission after changes to planning, markers, manual or automatic transitions, or their Kani harnesses.
---
# Verify transition geometry

Use an explicitly selected `funkot-autodj` working-set member. Read its
`AGENTS.md` and `verification/README.md`. The shared editing source is
`agent-rules/skills/verify-funkot-transition-geometry/SKILL.md`; the core's
`verification/SKILL.md` is a byte-identical distribution, reached from its
project instructions. Update the source and distribution together.

Last synchronization with product code is
`6067a0e6b029a1b3615f75fbd002ecb5e8a5ddfc`. This is an audit anchor, not a pin.
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
`finish_prepare`, and `Engine::from_prepared`. Inspect every new path that moves
their decks, entries, nudges or endpoints. The full target horizon comes from
the absolute main marker and corrected entry, including a real frame at main.
Manual rejection precedes queue/history consumption and start events.
Automatic fallback discards a rejected correction, then uses a bounded simple
fade or hard cut. Cached derived triggers are normalized by `cache.rs::read_entry`
without changing manual input fields or stored bytes.

## Run and judge

From the selected core root on Linux x86_64 with Docker and cgroup v2:

```sh
./scripts/check-transition-geometry.sh --setup
./scripts/check-transition-geometry.sh /tmp/transition-proof
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
and keep the last one-frame simple fade audible. Preserve those observable
boundaries when updating the checker after a code change.

## Limits and result record

The numeric proof does not verify floating-point conversion or that its caller
passes the correct musical marker. Those adapters are covered by engine
regressions. Phase scores, musical bar identity, click freedom, real music,
devices, realtime schedules, OS behavior and the #22 candidate-selection
question remain outside the proof. Run normal workspace checks and real-track
rendering as required by `AGENTS.md`; report unavailable input explicitly.

Record results, counterexamples, mutation failures, exact source/CI commits and
unproved scope in `funkot-autodj` Issue #26. Keep private paths, raw diagnostics
and per-file hashes in private evidence; link public CI artifacts for shared
logs. After changes that skipped this check, use
`maintain-formal-verification-skill` to classify each result as in sync, updated,
bug or blocked. Verification updates belong in a separate commit from product
fixes.
