"""Check the bounded navigation model and retain its counterexamples."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import resource
import signal
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.request

TLC_SHA256 = "936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88"
TLC_URL = "https://github.com/tlaplus/tlaplus/releases/download/v1.7.4/tla2tools.jar"
SOURCES = ("funkot-core/src/engine.rs", "funkot-core/src/lib.rs",
           "funkot-core/src/manual_navigation.rs", "verification/manual_navigation.rs",
           "verification/manual_navigation/ManualNavigation.tla",
           "verification/manual_navigation/ManualNavigation.cfg",
           "verification/manual_navigation/StableNavigation.cfg",
           "verification/check_manual_navigation.py")


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def source_revision(root):
    try:
        revision = subprocess.check_output(["git", "-C", str(root), "rev-parse", "--verify",
                                            "HEAD^{commit}"], text=True).strip()
    except subprocess.CalledProcessError as error:
        raise RuntimeError("cannot resolve source HEAD commit") from error
    if re.fullmatch(r"(?:[0-9a-f]{40}|[0-9a-f]{64})", revision) is None:
        raise RuntimeError("invalid source revision")
    try:
        dirty = subprocess.check_output(["git", "-C", str(root), "status", "--porcelain",
                                         "--untracked-files=normal"], text=True)
    except subprocess.CalledProcessError as error:
        raise RuntimeError("cannot inspect source status") from error
    if dirty:
        raise RuntimeError("source checkout is dirty; commit owned changes before verification")
    return revision


def run_tlc(tree, out, java, jar, label, cfg, started, violation=None):
    directory = tree / "verification/manual_navigation"
    config = directory / (label + ".cfg")
    config.write_text(cfg)
    command = [java, "-XX:MaxRAM=8g", "-XX:+UseParallelGC", "-cp", str(jar), "tlc2.TLC",
               "-workers", "auto" if violation is None else "1", "-seed", "0", "-fp", "0", "-config", config.name,
               "-metadir", str(tree / ("states-" + label)), "ManualNavigation.tla"]
    log_path = out / (label + ".log")
    with log_path.open("w") as log:
        process = subprocess.Popen(command, cwd=directory, stdout=log, stderr=subprocess.STDOUT,
                                   start_new_session=True)
        try:
            status = process.wait(timeout=max(0, 15 * 60 - (time.monotonic() - started)))
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
            raise RuntimeError(label + ": total verification deadline exceeded")
    log = log_path.read_text()
    counts = re.findall(r"([\d,]+) states generated, ([\d,]+) distinct states found, ([\d,]+) states left on queue", log)
    if not counts:
        raise RuntimeError(label + ": TLC produced no state counts")
    generated, distinct, pending = (int(value.replace(",", "")) for value in counts[-1])
    if not (generated >= distinct > 0):
        raise RuntimeError(label + ": invalid state counts")
    if violation is None:
        if status != 0 or pending != 0 or "Model checking completed. No error has been found." not in log:
            raise RuntimeError(label + ": TLC did not finish the complete safety and liveness check")
    elif status == 0 or violation not in log or "State 1:" not in log:
        raise RuntimeError(label + ": missing the required checked counterexample")
    print(label + (" PASS" if violation is None else " EXPECTED_FAIL"), flush=True)
    return {"status": "PASS" if violation is None else "EXPECTED_FAIL", "exit_code": status,
            "generated": generated, "distinct": distinct, "pending": pending}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--setup", action="store_true", help="download the fixed TLC jar and check its hash")
    parser.add_argument("--jar", type=Path, default=Path(os.environ.get("XDG_CACHE_HOME", Path.home() / ".cache")) / "funkot-verification/tla2tools-1.7.4.jar")
    parser.add_argument("--java", default="java", help="Java 21 executable")
    parser.add_argument("--output", type=Path, help="new or empty evidence directory")
    args = parser.parse_args()
    if args.setup:
        args.jar.parent.mkdir(parents=True, exist_ok=True)
        if not args.jar.exists():
            with tempfile.NamedTemporaryFile(dir=args.jar.parent, delete=False) as temporary:
                temporary_path = Path(temporary.name)
                with urllib.request.urlopen(TLC_URL) as response:
                    temporary.write(response.read())
            if sha256(temporary_path) != TLC_SHA256:
                raise RuntimeError("downloaded TLC jar does not match the fixed hash")
            temporary_path.replace(args.jar)
        if sha256(args.jar) != TLC_SHA256:
            raise RuntimeError("TLC jar does not match the fixed hash")
        print("TLC 1.7.4 ready")
        return 0
    root = Path(__file__).resolve().parents[1]
    out = args.output or Path(tempfile.mkdtemp(prefix="manual-navigation-proof-"))
    out.mkdir(parents=True, exist_ok=True)
    if any(out.iterdir()):
        parser.error("evidence directory is not empty; prior evidence is unchanged and no proof ran")
    summary = {"status": "FAIL"}
    summary_path = out / "summary.json"
    summary_path.write_text(json.dumps(summary) + "\n")
    started = time.monotonic()
    try:
        revision = source_revision(root)
        summary["revision"] = revision
        jar = args.jar.resolve(strict=True)
        if sha256(jar) != TLC_SHA256:
            raise RuntimeError("TLC jar does not match the fixed hash")
        java_version = subprocess.check_output([args.java, "-version"], stderr=subprocess.STDOUT, text=True)
        if re.search(r'version "21[.\"]', java_version) is None:
            raise RuntimeError("Java 21 is required")
        summary["tools"] = {"tlc": "1.7.4", "jar_sha256": TLC_SHA256,
                            "java": java_version.splitlines()[0]}
        with tempfile.TemporaryDirectory(prefix="manual-navigation-source-") as temporary:
            tree = Path(temporary)
            archive = subprocess.Popen(["git", "-C", str(root), "archive", "--format=tar", revision], stdout=subprocess.PIPE)
            with tarfile.open(fileobj=archive.stdout, mode="r|") as source:
                source.extractall(tree, filter="data")
            if archive.wait() != 0:
                raise RuntimeError("cannot archive the resolved source commit")
            summary["sources"] = {name: sha256(tree / name) for name in SOURCES}
            cfg = (tree / "verification/manual_navigation/ManualNavigation.cfg").read_text()
            summary["model"] = run_tlc(tree, out, args.java, jar, "baseline", cfg, started)
            stable_cfg = (tree / "verification/manual_navigation/StableNavigation.cfg").read_text()
            summary["stable_adoption"] = run_tlc(tree, out, args.java, jar, "stable", stable_cfg, started)
            summary["sensitivity"] = {}
            for label, constant, violation in (
                ("generation", "CheckGeneration", "Invariant GenerationSafe is violated"),
                ("start_frame", "CheckTiming", "Invariant ExactFrameSafe is violated"),
                ("deadline", "CheckDeadline", "Temporal properties were violated"),
                ("history_rollover", "CancelHistory", "Invariant AcceptedTargetSafe is violated"),
            ):
                anchor = constant + " = TRUE"
                if cfg.count(anchor) != 1:
                    raise RuntimeError(label + ": sensitivity edit no longer matches")
                mutated = cfg.replace(anchor, constant + " = FALSE")
                summary["sensitivity"][label] = run_tlc(tree, out, args.java, jar, label, mutated,
                                                       started, violation)
            summary["status"] = "PASS"
    except (OSError, RuntimeError, ValueError, subprocess.CalledProcessError, tarfile.TarError) as error:
        summary["error"] = str(error)
    summary.update(wall_seconds=round(time.monotonic() - started, 3),
                   peak_child_bytes=resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss * 1024)
    summary_path.write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary, indent=2))
    return 0 if summary["status"] == "PASS" else 1


if __name__ == "__main__":
    sys.exit(main())
