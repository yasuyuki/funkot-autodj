"""Check the fixed Kani result, including execution and reachable cover checks."""
import hashlib
import json
import os
import re
from pathlib import Path
import signal
import tempfile
import subprocess
import sys
import time


EXPECTED = {"plan_all_u32": 4, "trigger_all_u32": 5, "frame_admission_all_u64": 4,
            "manual_navigation_proofs::reply_is_current_all_u64_bool": 3,
            "manual_navigation_proofs::tick_decision_all_facts": 10,
            "first_live_proofs::upgrade_admission_all_facts": 6,
            "first_live_proofs::preview_wait_all_facts": 8,
            "first_live_proofs::failure_permit_transfer_all_variants": 2,
            "first_live_proofs::waiting_output_all_usize": 4}
started = time.monotonic()
result = Path("/results")
revision = os.environ.get("SOURCE_REVISION", "")
summary = {"status": "FAIL", "revision": revision}
(result / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
if re.fullmatch(r"(?:[0-9a-f]{40}|[0-9a-f]{64})", revision) is None:
    summary["error"] = "missing or invalid source revision"
    (result / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary))
    sys.exit(1)
(result / "kani.json").unlink(missing_ok=True)
from manual_navigation_mutations import NAVIGATION_MUTATIONS
from first_live_mutations import FIRST_LIVE_MUTATIONS
command = ["kani", "verification/transition_geometry.rs", "--target-dir", "/tmp/kani-proof",
           "-j", "1", "--output-format", "terse", "-Z", "unstable-options",
           "--export-json", str(result / "kani.json")]
limit = int(Path("/sys/fs/cgroup/memory.max").read_text())
assert 0 < limit <= 8 * 1024**3, "verification requires a total 8 GiB cgroup limit"
with (result / "kani.log").open("w") as log:
    process = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    try:
        status = process.wait(timeout=15 * 60)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()
        status = 124
elapsed = time.monotonic() - started
peak = int(Path("/sys/fs/cgroup/memory.peak").read_text())
summary = {"status": "FAIL", "exit_code": status, "wall_seconds": round(elapsed, 3),
           "peak_bytes": peak, "limit_bytes": limit,
           "revision": revision,
           "sources": {name: hashlib.sha256(Path(name).read_bytes()).hexdigest() for name in
                       ("funkot-core/src/transition_geometry.rs", "verification/transition_geometry.rs",
                        "funkot-core/src/manual_navigation.rs", "verification/manual_navigation.rs",
                        "funkot-core/src/engine.rs", "verification/manual_navigation_mutations.py",
                        "funkot-core/src/first_live.rs", "verification/first_live.rs",
                        "verification/first_live_mutations.py")}}
try:
    assert status == 0, f"Kani exited {status} (timeout/resource/verification failure is not PASS)"
    data = json.loads((result / "kani.json").read_text())
    assert data["tools"]["kani"] == "0.68.0", "unexpected verifier version"
    assert data["tools"]["rustc"] == "rustc 1.100.0-nightly (8925ea358 2026-08-20)", "unexpected bundled Rust"
    assert data["tools"]["cbmc"] == "6.11.0 (cbmc-6.11.0)", "unexpected CBMC version"
    verification = data["verification_results"]
    assert verification["summary"]["status"] == "completed"
    assert verification["summary"]["executed"] == len(EXPECTED)
    results = verification["results"]
    assert len(results) == len(EXPECTED)
    assert {r["harness_id"] for r in results} == set(EXPECTED), "missing/extra harness"
    for r in results:
        assert r["status"] == "Success", r["harness_id"]
        checks = r["checks"]
        covers = [c for c in checks if c["category"] == "cover"]
        assert len(covers) == EXPECTED[r["harness_id"]]
        assert all(c["status"] == "Satisfied" for c in covers), "unreachable cover"
        assert all(c["status"] in {"Success", "Unreachable", "Satisfied"} for c in checks)
    summary["sensitivity"] = {}
    geometry = "funkot-core/src/transition_geometry.rs"
    navigation = "funkot-core/src/manual_navigation.rs"
    first_live = "funkot-core/src/first_live.rs"
    mutations = (
        ("plan_all_u32", "plan_all_u32", geometry,
         (("let skip = i.saturating_sub(m)", "let skip = i"),)),
        ("trigger_all_u32", "trigger_all_u32", geometry,
         (("saturating_add(OUTRO_LEAD_BARS)", "saturating_add(0)"),)),
        ("frame_admission_all_u64", "frame_admission_all_u64", geometry,
         (("fade_out_end > prev_frames - prev_start", "false"),)),
    ) + tuple((label, harness, navigation, edits) for label, harness, edits, _ in NAVIGATION_MUTATIONS) + tuple(
        (label, harness, first_live, edits) for label, harness, edits, _ in FIRST_LIVE_MUTATIONS)
    for label, harness, source_name, edits in mutations:
        text = Path(source_name).read_text()
        for original, broken in edits:
            assert text.count(original) == 1, f"{label}: mutation no longer matches"
            text = text.replace(original, broken)
        with tempfile.TemporaryDirectory(prefix="transition-sensitivity-") as temporary:
            tree = Path(temporary)
            for name in ("verification/transition_geometry.rs", "verification/manual_navigation.rs",
                         "verification/first_live.rs", geometry, navigation, first_live):
                target = tree / name
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(Path(name).read_bytes())
            (tree / source_name).write_text(text)
            mutation_json = result / f"{label}-sensitivity.json"
            mutation_json.unlink(missing_ok=True)
            mutation_command = ["kani", "verification/transition_geometry.rs", "--harness", harness,
                                "--target-dir", str(tree / "target"), "-j", "1",
                                "--output-format", "terse", "-Z", "unstable-options",
                                "--export-json", str(mutation_json)]
            with (result / f"{label}-sensitivity.log").open("w") as log:
                process = subprocess.Popen(mutation_command, cwd=tree, stdout=log,
                                           stderr=subprocess.STDOUT, start_new_session=True)
                try:
                    mutation_status = process.wait(timeout=max(0, 15 * 60 - (time.monotonic() - started)))
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()
                    mutation_status = 124
            assert mutation_status not in (0, 124), f"{harness}: mutation passed or timed out"
            mutant = json.loads(mutation_json.read_text())["verification_results"]
            assert mutant["summary"]["status"] == "completed"
            assert mutant["summary"]["executed"] == 1
            checks = mutant["results"]
            assert len(checks) == 1 and checks[0]["harness_id"] == harness
            assert checks[0]["status"] == "Failure"
            assert any(c["status"] == "Failure" and c["category"] == "assertion"
                       for c in checks[0]["checks"]), f"{harness}: no safety counterexample"
            summary["sensitivity"][label] = "EXPECTED_FAIL"
    assert all(hashlib.sha256(Path(name).read_bytes()).hexdigest() == digest
               for name, digest in summary["sources"].items())
    summary.update(status="PASS", tools=data["tools"], harnesses=EXPECTED)
except (AssertionError, KeyError, ValueError, TypeError, IndexError, OSError) as error:
    summary["error"] = str(error)
    print((result / "kani.log").read_text())
summary.update(wall_seconds=round(time.monotonic() - started, 3),
               peak_bytes=int(Path("/sys/fs/cgroup/memory.peak").read_text()))
(result / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
print(json.dumps(summary, indent=2))
sys.exit(0 if summary["status"] == "PASS" else 1)
