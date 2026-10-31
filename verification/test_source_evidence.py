"""Exercise the public runner with real Git clones and linked worktrees."""
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
        (evidence / "summary.json").write_text('{"status":"PASS","revision":"stale"}')
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
