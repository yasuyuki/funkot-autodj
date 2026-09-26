"""Run with python3 -B -m unittest discover -s tools -p test_dev_owner_context.py."""
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from dev_owner_context import bindings, registration


class OwnerBridgeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name) / 'checkout'
        self.repo.mkdir()
        subprocess.run(['git', 'init', '-q', str(self.repo)], check=True)
        self.common = self.repo / '.git'
        self.receipts = self.common / 'workspace-lifecycle' / 'owner-receipts' / hashlib.sha256(b'task').hexdigest()
        self.receipts.mkdir(parents=True)
        self.context = {
            'repo': str(self.repo), 'task': 'task',
            'owner_receipt_dir': str(self.receipts),
            'owner_receipt_argv': [sys.executable, '-m', 'workspace_lifecycle', '--repo',
                                   str(self.repo), 'register-owner-receipt', '--task', 'task'],
        }
        self.context, _, _ = bindings(json.dumps(self.context), self.repo)
        self.output = self.repo / 'new.wav'
        self.receipt = self.receipts / '.g123.owned'
        self.receipt.write_text(json.dumps({'owner': 'funkot-wav', 'generation': 'g123',
            'output': str(self.output), 'receipt': str(self.receipt), 'state': 'writing', 'identity': None}))
        completion = [*self.context['owner_completion_argv'], '--artifact-complete', str(self.output),
                      '--generation', 'g123', '--artifact-receipt', str(self.receipt),
                      '--accepted-proof', '{result_ref}', '--released-proof', '{result_ref}']
        self.argv = ['--owner', 'funkot-wav', '--generation', 'g123', '--output', str(self.output),
                     '--receipt', str(self.receipt), '--completion-json', json.dumps(completion)]

    def test_exact_request_keeps_host_interpreter_and_prefix(self):
        command = registration(self.argv, self.context, self.repo, self.receipts)
        self.assertEqual(command, [*self.context['owner_receipt_argv'], *self.argv])
        self.assertEqual(self.context['owner_completion_argv'][0], str(self.repo / 'dev.sh'))

    def test_foreign_task_or_common_binding_refused(self):
        for field in ('repo', 'owner_receipt_dir'):
            context = dict(self.context, **{field: self.temp.name})
            with self.assertRaises(ValueError):
                bindings(json.dumps(context), self.repo)

    def test_arbitrary_command_protocol_and_callback_refused(self):
        cases = [['exec', 'sh'], [*self.argv, '--extra', 'x'],
                 [*self.argv[:-1], json.dumps(['sh', '-c', 'bad'])]]
        for argv in cases:
            with self.assertRaises(ValueError):
                registration(argv, self.context, self.repo, self.receipts)

    def test_existing_manual_output_refused(self):
        self.output.write_bytes(b'manual')
        with self.assertRaises(ValueError):
            registration(self.argv, self.context, self.repo, self.receipts)
        self.assertEqual(self.output.read_bytes(), b'manual')

    def test_foreign_output_receipt_and_symlink_refused(self):
        for index, value in [(5, str(Path(self.temp.name) / 'outside.wav')),
                             (7, str(self.repo / 'wrong.receipt'))]:
            argv = list(self.argv); argv[index] = value
            with self.assertRaises(ValueError):
                registration(argv, self.context, self.repo, self.receipts)
        self.output.symlink_to(self.repo / 'target')
        with self.assertRaises(ValueError):
            registration(self.argv, self.context, self.repo, self.receipts)

    def test_nonwriting_or_nonmatching_receipt_refused(self):
        claim = json.loads(self.receipt.read_text())
        for field, value in [('state', 'held'), ('generation', 'g999'), ('identity', {'ino': 1})]:
            self.receipt.write_text(json.dumps(dict(claim, **{field: value})))
            with self.assertRaises(ValueError):
                registration(self.argv, self.context, self.repo, self.receipts)


if __name__ == '__main__':
    unittest.main()
