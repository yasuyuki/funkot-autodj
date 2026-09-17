#!/usr/bin/env python3
"""Execute the workflow's gate against fresh and old synthetic Git history."""
import os
from pathlib import Path
import re
import subprocess
import tempfile
import textwrap
import unittest

WORKFLOW = Path(__file__).resolve().parents[1] / '.github/workflows/ci.yml'


class ScheduleGate(unittest.TestCase):
    def test_schedule_age_and_manual_audit_selection(self):
        workflow = WORKFLOW.read_text()
        gate = workflow.split('  gate:', 1)[1].split('  test:', 1)[0]
        script = textwrap.dedent(re.search(r'        run: \|\n((?:          .*\n|\n)+)', gate)[1])
        for old in (False, True):
            with tempfile.TemporaryDirectory() as scratch:
                root = Path(scratch)
                subprocess.run(['git', 'init', '-q', '-b', 'master', str(root)], check=True)
                env = os.environ.copy()
                if old:
                    env.update(GIT_AUTHOR_DATE='2000-01-01T00:00:00Z', GIT_COMMITTER_DATE='2000-01-01T00:00:00Z')
                subprocess.run(['git', '-c', 'user.name=CI test', '-c', 'user.email=ci@example.invalid',
                                'commit', '-q', '--allow-empty', '-m', 'synthetic history'], cwd=root, env=env, check=True)
                for event, audit_only in (('push', ''), ('pull_request', ''),
                                          ('workflow_dispatch', 'true'),
                                          ('workflow_dispatch', 'false'), ('schedule', '')):
                    with self.subTest(old=old, event=event, audit_only=audit_only):
                        output = root / 'output'
                        output.write_text('')
                        env['GITHUB_OUTPUT'] = str(output)
                        subprocess.run(['bash', '-eu', '-c', script.replace('${{ github.event_name }}', event).replace('${{ inputs.audit_only }}', audit_only)],
                                       cwd=root, env=env, check=True)
                        skip = (old and event == 'schedule') or (event == 'workflow_dispatch' and audit_only == 'true')
                        expected = 'false' if skip else 'true'
                        self.assertEqual(output.read_text().strip(), f'run={expected}')

    def test_audit_is_independent_and_heavy_jobs_remain_gated(self):
        workflow = WORKFLOW.read_text()
        jobs = dict(re.findall(r'^  ([\w-]+):\n(.*?)(?=^  [\w-]+:|\Z)',
                               workflow.split('jobs:\n', 1)[1], re.M | re.S))
        audit = jobs['dependency-policy']
        self.assertNotRegex(audit, r'(?m)^    (needs|if):')
        self.assertIn('command-arguments: advisories licenses sources', audit)
        self.assertIn('arguments: --locked', audit)
        self.assertNotIn('continue-on-error', audit)
        self.assertNotRegex(audit, r'cargo (build|test)|dev\.sh|cross-build')
        self.assertIn('python3 scripts/test-ci-gate.py', audit)
        for name in ('test', 'windows-cache', 'package'):
            self.assertIn("needs.gate.outputs.run == 'true'", jobs[name])
            self.assertRegex(jobs[name], r'needs: \[gate[,\]]')
        self.assertIn('dependency-policy]', jobs['package'])
        self.assertIn("github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v')", jobs['release'])
        self.assertIn('needs: [package]', jobs['release'])
        self.assertRegex(workflow, r'audit_only:\n(?:.*\n)*?        type: boolean\n        default: true')

    def test_ci_and_docker_rust_versions_agree(self):
        workflow = WORKFLOW.read_text()
        version = re.search(r'RUST_VERSION: "([^"]+)"', workflow)[1]
        dockerfiles = sorted(WORKFLOW.parents[2].glob('Dockerfile*'))
        self.assertTrue(dockerfiles)
        for dockerfile in dockerfiles:
            with self.subTest(file=dockerfile.name):
                versions = re.findall(r'^FROM rust:([0-9.]+)-', dockerfile.read_text(), re.M)
                self.assertEqual(versions, [version])
        toolchains = re.findall(r'toolchain: "([^"]+)"', workflow)
        self.assertTrue(toolchains)
        self.assertEqual(set(toolchains), {'${{ env.RUST_VERSION }}'})


if __name__ == '__main__':
    unittest.main()
