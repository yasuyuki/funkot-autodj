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

EXPECTED = {"plan_all_u32": 4, "trigger_all_u32": 5, "frame_admission_all_u64": 4}
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
command = ["kani", "verification/transition_geometry.rs", "--target-dir", "/tmp/kani-proof",
           "-j", "1", "--output-format", "terse", "-Z", "unstable-options",
           "--export-json", str(result / "kani.json")]
# The container limit covers the compiler, verifier and all solver processes.
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
                       ("funkot-core/src/transition_geometry.rs", "verification/transition_geometry.rs")}}
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
    source = Path("funkot-core/src/transition_geometry.rs")
    original_source = source.read_text()
    mutations = (
        ("plan_all_u32", "let skip = i.saturating_sub(m)", "let skip = i"),
        ("trigger_all_u32", "saturating_add(OUTRO_LEAD_BARS)", "saturating_add(0)"),
        ("frame_admission_all_u64", "fade_out_end > prev_frames - prev_start", "false"),
    )
    for harness, original, broken in mutations:
        assert original_source.count(original) == 1, f"{harness}: mutation no longer matches"
        with tempfile.TemporaryDirectory(prefix="transition-sensitivity-") as temporary:
            tree = Path(temporary)
            (tree / "verification").mkdir()
            (tree / "funkot-core/src").mkdir(parents=True)
            (tree / "verification/transition_geometry.rs").write_bytes(
                Path("verification/transition_geometry.rs").read_bytes())
            (tree / source).write_text(original_source.replace(original, broken))
            mutation_json = result / f"{harness}-sensitivity.json"
            mutation_json.unlink(missing_ok=True)
            mutation_command = ["kani", "verification/transition_geometry.rs", "--harness", harness,
                                "--target-dir", str(tree / "target"), "-j", "1",
                                "--output-format", "terse", "-Z", "unstable-options",
                                "--export-json", str(mutation_json)]
            with (result / f"{harness}-sensitivity.log").open("w") as log:
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
            summary["sensitivity"][harness] = "EXPECTED_FAIL"
    assert hashlib.sha256(source.read_bytes()).hexdigest() == summary["sources"][str(source)]
    summary.update(status="PASS", tools=data["tools"], harnesses=EXPECTED)
except (AssertionError, KeyError, ValueError, OSError) as error:
    summary["error"] = str(error)
    print((result / "kani.log").read_text())
summary.update(wall_seconds=round(time.monotonic() - started, 3),
               peak_bytes=int(Path("/sys/fs/cgroup/memory.peak").read_text()))
(result / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
print(json.dumps(summary, indent=2))
sys.exit(0 if summary["status"] == "PASS" else 1)
