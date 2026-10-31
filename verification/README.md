# Transition geometry

The driver imports `funkot-core/src/transition_geometry.rs` directly. The engine
and analysis modules call that same source. Kani dependencies stay outside the
release Cargo workspace, Android build and C ABI.

From the core repository root on Linux x86_64 with Docker and cgroup v2:

```sh
./scripts/check-transition-geometry.sh --setup
./scripts/check-transition-geometry.sh
```

The fixed verifier is Kani 0.68.0, bundled Rust nightly-2026-08-21 and CBMC
6.11.0. The runner reuses the player verification entry's 15-minute deadline
and 8-GiB cgroup limit without extra swap. `summary.json` retains elapsed time,
peak memory, revision and source hashes. `kani.json` retains individual checks.
The runner requires a readable Git checkout with a valid HEAD commit and no
tracked or nonignored untracked changes. This applies to clones and linked
worktrees. Use a new or empty evidence directory outside the checkout or in an
ignored directory. A nonempty directory is refused without changing prior results.
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

## Manual navigation

`manual_navigation.rs` imports the production `manual_navigation` module.
Kani checks all `u64` generations and frame values, independent identity facts,
optional active decks and plans, and deadlines with or without an available
fallback. It proves current-reply admission, exact-frame adoption, deadline
precedence, and late-plan replan or fallback. Its two harnesses have 3 and 10
reachable covers. They run in the existing geometry job, with its existing
three numeric harnesses and resource limits.

The identity fact comes from the Engine's borrowed `ManualKey` comparisons.
The proof does not establish that a caller constructed that fact correctly.
Real Engine tests replace active and target PCM without a generation change,
replay delayed Restart replies, and reject an old reply after fallback. Each
of the generation, identity, deadline and exact-start mutations must fail both
a completed Kani safety assertion and its named Engine regression. The native
identity variant also disables the Engine's independent identity recheck;
removing only the first predicate leaves that final defense intact. Mutation
edits and regression names share `manual_navigation_mutations.py`.

The separate TLC check enumerates event order. On Linux with Python 3.12 or
later and Java 21, run from the repository root:

```sh
python3 -B verification/check_manual_navigation.py --setup
python3 -B verification/check_manual_navigation.py
```

The setup downloads the fixed TLC 1.7.4 jar and checks its SHA-256. The run
requires a clean committed checkout and a new evidence directory, then checks
a commit archive. `--help` provides explicit Java, jar and evidence locations.
The normal command runs safety, conditional progress and negative variants.
Timeout, missing state counts, a nonempty search queue, an incomplete run or
a negative variant without its specified counterexample fails the command.
The existing 15-minute verification deadline covers the complete run. Java
uses an 8-GiB MaxRAM configuration; this is a JVM sizing input, not a cgroup
process-tree limit. The summary records the Java version, jar hash, source
revision and hashes, state counts, elapsed time and peak child RSS.

### Bounds and assumptions

The model has two accepted-operation identifiers, two source epochs, four
occurrence identities and two PCM identities per occurrence. Four identities distinguish history A, outgoing B, active C and an independent
ready-next D. Three suffice for the actual history counterexample in #44,
but aliasing D to B loses the Next-to-Next distinct-target case. Ready outcomes
cover a fresh occurrence and a repeated current occurrence with different PCM.
Fresh labels are canonical representatives outside the live occurrence set;
other aliases with older history entries are outside this bounded model.
It includes two successive commands, at most two preparation attempts per
accepted operation, and zero, one or two copies of each reply. This covers one
replan and delayed, reordered, lost or duplicate replies. The current worker
normally sends one reply; duplication is a robustness overapproximation.

The clock has three ordered positions within the current active body. A new
adoption resets it. Plan start and the fixed operation deadline remain separate.
This is an order abstraction, not a claim about audio callback duration.
Generations are never reused within the model; `GenerationOrder` checks the
monotone accepted-operation identifier. Integer wraparound is outside TLC.

Preparation is abstracted as an eventual mailbox outcome at dispatch. Arbitrary
poll delay represents a slow worker; zero copies represent loss. Worker failure
prevents further dispatch. The model permits dispatch earlier than a busy real
worker, so it covers additional event orders. It does not equate worker execution
with the Engine's `manual_in_flight` flag. A stale reply poll is an Engine-observable stutter. Its only scheduling effect,
clearing `manual_in_flight`, is already covered by earlier abstract dispatch.
After generation invalidation, the model removes these unobservable old replies.
No dispatch capacity, deadline, invariant or fair Advance/Tick action depends on
their count. The shared Rust predicate proof and stale-reply Engine regressions
justify this boundary. The generation mutation keeps the full stale payload.
No TLC VIEW quotient removes mailbox exits.

Engine changes are serialized. Reply checks and adoption form one indivisible
step. Accept/dispatch and poll/dispatch are split conservatively, admitting
additional mailbox scheduling orders rather than parallel Engine mutations.
A counterexample from that overapproximation requires replay against Rust.

The ordinary config checks safety across the complete bounded event graph.
Progress is checked separately in the stable config, which requires adoption
under fair frame advance and ticking, successful admission, stable identities,
and no superseding command, stop, cancellation, upgrade or source replacement.
Neither result proves progress for an unbounded or permanently stopped system.

### Code correspondence

| Model action | Product operation and observable boundary |
|---|---|
| `SendNav`, `DrainNav` | `drain_nav_commands` and `begin_nav` coalesce normal commands, defer them during a future fence, and preserve duplicate Next's target/deadline. |
| `Dispatch`, `PollReply`, `WorkerFailure` | `kick_manual_plan_if_idle`, the worker channel and `poll_nav_tempo` choose a current reply or discard it. Zero/two copies abstract loss/duplication. |
| `Tick`, `DeadlineFallback` | `tick_decision` and `tick_pending_nav` give an exact reply precedence at its deadline, replan late work without extending the deadline, or make a current simple fallback. |
| `Apply` | `commit_manual_plan` checks identity and `admit_frames` before taking next/history or emitting a start, invalidates rejected work, then `begin_transition_to` installs admitted decks atomically. |
| `Advance` | `render_one_frame` advances the active playhead. DSP and frame lengths are abstracted away. |
| `Cancel`, `Stop` | `cancel_pending_nav` and `stop` invalidate outstanding work. |
| `UpgradeActive`, `UpgradeNext`, `ReadyNext` | `drain_loader` and `apply_upgrade` adopt current loader messages. The future fence prevents those adoptions. |
| `HistoryRollover` | `drop_prev` and `push_history` retain the actual outgoing deck, skip a shared Restart deck, and cancel pending Prev before replacing its target. |
| `FutureBegin`, `FutureAbort`, `FutureCommit` | `begin_future_update`, `abort_future_update` and `commit_future_update` preserve or replace future state and invalidate old navigation. Persistence is outside this model. |

The safety invariants cover generation/source/active/target identity at adoption,
accepted target stability, exact reply start, adoption once, start/consumption
correspondence, and no future adoption while fenced. Admission rejection and
worker failure leave decks and consumption unchanged. The source epoch is
invalidated through navigation generation, as in the product; the model does
not invent a source-epoch field in `ManualKey`.

The deliberate TLC variants remove generation checking, exact-start checking,
deadline fallback, and history-target cancellation. Each must produce its
specified invariant or temporal counterexample. Identity checking is separately
broken in the real shared Rust predicate and Engine regression. In the bounded
protocol model, correct invalidation can make that defensive check redundant.

Read TLC results with `/statechart`. The history trace is the A-to-B-to-C,
accepted-Prev-to-A, history-rollover-to-B sequence replayed by
`history_rollover_cancels_an_accepted_previous_navigation`. The three-line
product correction and its regression are separate commits in PR #44.

Record navigation results in #41. TLC does not verify PCM samples, DSP quality,
click freedom, OS scheduling, device output, unbounded request streams or the
preview/full first-live lifecycle. Loader identities and future fencing are
represented only at their navigation boundaries. The separate work in player
#112 and core #42 must establish their own persistence and upgrade promises.
