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
    def test_only_schedule_can_skip_old_commits(self):
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
                for event in ('push', 'pull_request', 'workflow_dispatch', 'schedule'):
                    with self.subTest(old=old, event=event):
                        output = root / 'output'
                        output.write_text('')
                        env['GITHUB_OUTPUT'] = str(output)
                        subprocess.run(['bash', '-eu', '-c', script.replace('${{ github.event_name }}', event)],
                                       cwd=root, env=env, check=True)
                        expected = 'false' if old and event == 'schedule' else 'true'
                        self.assertEqual(output.read_text().strip(), f'run={expected}')


if __name__ == '__main__':
    unittest.main()
