import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
GIT = shutil.which("git")


class SourceEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.repo = self.base / "source"
        self.repo.mkdir()
        for name in ("scripts/check-transition-geometry.sh", "verification/check.py"):
            target = self.repo / name
            target.parent.mkdir(exist_ok=True)
            shutil.copyfile(ROOT / name, target)
        (self.repo / "production.rs").write_text("committed source\n")
        self.git("init", "-q")
        self.git("config", "user.name", "Proof fixture")
        self.git("config", "user.email", "proof@example.invalid")
        self.git("add", "scripts", "verification", "production.rs")
        self.git("commit", "-qm", "fixture")
        self.revision = self.git("rev-parse", "HEAD").strip()
        self.bin = self.base / "bin"
        self.bin.mkdir()
        docker = self.bin / "docker"
        docker.write_text("""#!/usr/bin/env python3
import json, os, pathlib, sys
out = pathlib.Path(os.environ['PROBE'])
out.with_suffix('.argv').write_text(json.dumps(sys.argv))
out.write_bytes(sys.stdin.buffer.read())
sys.exit(23)
""")
        docker.chmod(0o755)
        self.env = dict(os.environ, PATH=str(self.bin) + os.pathsep + os.environ["PATH"],
                        PROBE=str(self.base / "archive"), SOURCE_REVISION="forged")

    def git(self, *args, cwd=None):
        return subprocess.check_output([GIT, "-C", str(cwd or self.repo), *args], text=True,
                                       stderr=subprocess.STDOUT)

    def checkout(self, linked):
        if not linked:
            return self.repo
        target = self.base / "linked"
        self.git("worktree", "add", "-qb", "linked-fixture", str(target))
        return target

    def run_entry(self, checkout):
        evidence = self.base / "evidence"
        evidence.mkdir(exist_ok=True)
        previous = evidence / "summary.json"
        if previous.exists():
            previous.unlink()
        result = subprocess.run(["bash", str(checkout / "scripts/check-transition-geometry.sh"),
                                 str(evidence)], env=self.env, text=True, capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        summary = json.loads((evidence / "summary.json").read_text())
        self.assertEqual(summary["status"], "FAIL")
        return summary

    def test_clean_clone_and_linked_worktree_archive_exact_commit(self):
        for linked in (False, True):
            with self.subTest(linked=linked):
                checkout = self.checkout(linked)
                self.run_entry(checkout)
                archive = self.base / "archive"
                self.assertTrue(archive.exists(), "clean source must reach Docker")
                argv = json.loads(archive.with_suffix('.argv').read_text())
                self.assertIn("SOURCE_REVISION=" + self.revision, argv)
                with tarfile.open(fileobj=io.BytesIO(archive.read_bytes())) as tree:
                    self.assertEqual(tree.extractfile("production.rs").read(), b"committed source\n")
                    self.assertNotIn(".git", tree.getnames())

    def test_dirty_worktree_index_and_untracked_source_are_rejected(self):
        for linked in (False, True):
            checkout = self.checkout(linked)
            for change in ("worktree", "index", "untracked"):
                with self.subTest(linked=linked, change=change):
                    source = checkout / ("extra.rs" if change == "untracked" else "production.rs")
                    original = source.read_bytes() if source.exists() else None
                    source.write_text("uncommitted source\n")
                    if change == "index":
                        self.git("add", "production.rs", cwd=checkout)
                    summary = self.run_entry(checkout)
                    self.assertIn("dirty", summary["error"])
                    self.assertFalse((self.base / "archive").exists())
                    if original is None:
                        source.unlink()
                    else:
                        source.write_bytes(original)
                    if change == "index":
                        self.git("add", "production.rs", cwd=checkout)

    def test_unresolvable_clone_and_linked_git_metadata_fail_closed(self):
        for linked in (False, True):
            with self.subTest(linked=linked):
                checkout = self.checkout(linked)
                metadata = checkout / ".git"
                saved = checkout / ".git-unavailable"
                metadata.rename(saved)
                if linked:
                    metadata.write_text("gitdir: /missing-proof-fixture/worktrees/linked\n")
                summary = self.run_entry(checkout)
                self.assertIn("resolve", summary["error"])
                self.assertFalse((self.base / "archive").exists())
                if linked:
                    metadata.unlink()
                saved.rename(metadata)

    def test_invalid_revision_and_status_failure_never_start_docker(self):
        shim = self.bin / "git"
        for variant in ("invalid", "empty", "status-failure"):
            with self.subTest(variant=variant):
                shim.write_text("#!/usr/bin/env python3\nimport os,sys\n"
                    + ("if 'status' in sys.argv: sys.exit(71)\n" if variant == "status-failure" else
                       "if 'rev-parse' in sys.argv: print(" + repr("" if variant == "empty" else "not-a-revision") + "); sys.exit(0)\n")
                    + "os.execv(" + repr(GIT) + ", [" + repr(GIT) + ", *sys.argv[1:]])\n")
                shim.chmod(0o755)
                self.run_entry(self.repo)
                self.assertFalse((self.base / "archive").exists())

    def test_validator_rejects_missing_and_malformed_revision_before_kani(self):
        for revision in (None, "", " ", "not-a-revision", "a" * 39, "a" * 41,
                         "g" * 40, "a" * 40 + "\n"):
            with self.subTest(revision=revision):
                target = self.base / "validator"
                target.mkdir(exist_ok=True)
                env = self.env.copy()
                if revision is None:
                    env.pop("SOURCE_REVISION", None)
                else:
                    env["SOURCE_REVISION"] = revision
                program = """import pathlib,runpy,sys
from unittest.mock import patch
real_path=pathlib.Path
with patch('pathlib.Path', side_effect=lambda p: real_path(sys.argv[2]) if p == '/results' else real_path(p)):
    runpy.run_path(sys.argv[1], run_name='__main__')
"""
                result = subprocess.run(["python3", "-c", program, str(ROOT / "verification/check.py"),
                                         str(target)], env=env, capture_output=True, text=True)
                self.assertEqual(result.returncode, 1, result.stderr)
                summary = json.loads((target / "summary.json").read_text())
                self.assertEqual(summary["status"], "FAIL")
                self.assertIn("revision", summary["error"])
                self.assertFalse((target / "kani.log").exists())

    def test_reused_evidence_is_refused_without_destroying_previous_results(self):
        evidence = self.base / "evidence"
        evidence.mkdir()
        for name in ("summary.json", "kani.json", "plan_all_u32-sensitivity.json", "kani.log"):
            (evidence / name).write_text("previous proof evidence")
        result = subprocess.run(["bash", str(self.repo / "scripts/check-transition-geometry.sh"),
                                 str(evidence)], env=self.env, capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no proof ran", result.stderr)
        self.assertFalse((self.base / "archive").exists())
        for path in evidence.iterdir():
            self.assertEqual(path.read_text(), "previous proof evidence")

    def test_wrong_shaped_kani_json_records_a_validation_failure(self):
        tools = {"kani": "0.68.0", "rustc": "rustc 1.100.0-nightly (8925ea358 2026-08-20)",
                 "cbmc": "6.11.0 (cbmc-6.11.0)"}
        for payload in ({"tools": []}, {"tools": tools, "verification_results": []}):
            with self.subTest(payload=payload):
                target = self.base / "shape-result"
                target.mkdir(exist_ok=True)
                (target / "memory.max").write_text(str(8 * 1024**3))
                (target / "memory.peak").write_text("1")
                env = dict(self.env, SOURCE_REVISION="a" * 40)
                program = """import json,pathlib,runpy,sys
from unittest.mock import patch
real_path=pathlib.Path
out=real_path(sys.argv[2])
def resolve(p):
    if p == '/results': return out
    if str(p).startswith('/sys/fs/cgroup/'): return out / str(p).split('/')[-1]
    return real_path(p)
class Process:
    def __init__(self,*args,**kwargs):
        (out / 'kani.json').write_text(sys.argv[3])
    def wait(self,**kwargs): return 0
with patch('pathlib.Path', side_effect=resolve), patch('subprocess.Popen', Process):
    runpy.run_path(sys.argv[1], run_name='__main__')
"""
                result = subprocess.run(["python3", "-c", program, str(ROOT / "verification/check.py"),
                                         str(target), json.dumps(payload)], cwd=ROOT, env=env,
                                        capture_output=True, text=True)
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertNotIn("Traceback", result.stderr)
                summary = json.loads((target / "summary.json").read_text())
                self.assertEqual(summary["status"], "FAIL")
                self.assertTrue(summary["error"])
