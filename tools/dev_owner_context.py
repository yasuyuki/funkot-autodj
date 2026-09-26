#!/usr/bin/env python3
"""Run-scoped owner registration bridge for the existing dev.sh container.

The host keeps the lifecycle lease and executes only its configured registration
command. A container can register exact new WAV generations, never request an
arbitrary host command. There is no persistent daemon or global registry.
"""
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import socketserver
import subprocess
import sys
import tempfile
import threading


def no_links(value, *, exists=True):
    path = Path(value)
    if not path.is_absolute() or '..' in path.parts:
        raise ValueError('owner paths must be absolute without parent traversal')
    for part in (path, *path.parents):
        if part.is_symlink():
            raise ValueError('owner path contains a symlink')
    if exists and not path.exists():
        raise ValueError('owner binding does not exist')
    return path


def bindings(raw, cwd):
    context = json.loads(raw)
    cwd = Path(cwd).resolve(strict=True)
    if no_links(context['repo']) != cwd:
        raise ValueError('managed dev.sh must run in its bound task checkout')
    receipts = no_links(context['owner_receipt_dir'])
    if not receipts.is_dir():
        raise ValueError('owner state binding is not a directory')
    actual_common = Path(subprocess.check_output(
        ['git', 'rev-parse', '--path-format=absolute', '--git-common-dir'],
        cwd=cwd, text=True).strip()).resolve(strict=True)
    expected_receipts = actual_common / 'workspace-lifecycle' / 'owner-receipts' / hashlib.sha256(context['task'].encode()).hexdigest()
    if receipts != expected_receipts:
        raise ValueError('owner receipt directory does not belong to this exact task')
    argv = context['owner_receipt_argv']
    if not isinstance(argv, list) or not all(isinstance(a, str) for a in argv):
        raise ValueError('owner registration must be an argv list')
    expected = ['-m', 'workspace_lifecycle', '--repo', str(cwd),
                'register-owner-receipt', '--task', context['task']]
    if argv[1:] != expected:
        raise ValueError('owner registration prefix is not the advertised task command')
    completion = context.get('owner_completion_argv', [str(cwd / 'dev.sh'), 'cargo', 'run',
                              '--release', '-p', 'funkot-cli', '--'])
    if not isinstance(completion, list) or not completion or not all(isinstance(a, str) for a in completion):
        raise ValueError('owner completion must be a nonempty argv list')
    context['owner_completion_argv'] = completion
    return context, cwd, receipts


def registration(argv, context, cwd, receipts):
    """Validate a fixed register-only request before invoking the host owner."""
    keys = ['--owner', '--generation', '--output', '--receipt', '--completion-json']
    if not isinstance(argv, list) or len(argv) != 2 * len(keys) or argv[::2] != keys:
        raise ValueError('only the exact WAV generation registration protocol is supported')
    if not all(isinstance(a, str) for a in argv):
        raise ValueError('registration values must be strings')
    owner, generation, output, receipt, completion_json = argv[1::2]
    if owner != 'funkot-wav' or not generation.startswith('g') or not all(c.isascii() and (c.isalnum() or c == '-') for c in generation):
        raise ValueError('invalid WAV owner or generation')
    output = no_links(output, exists=False)
    receipt = no_links(receipt)
    if cwd not in output.parents or receipt.parent != receipts:
        raise ValueError('output or receipt is outside the declared owner mounts')
    if output.exists() or receipt.name != '.' + generation + '.owned':
        raise ValueError('output already exists or receipt does not match generation')
    expected = [*context['owner_completion_argv'], '--artifact-complete', str(output),
                '--generation', generation, '--artifact-receipt', str(receipt),
                '--accepted-proof', '{result_ref}', '--released-proof', '{result_ref}']
    if json.loads(completion_json) != expected:
        raise ValueError('completion argv differs from the host owner contract')
    claim = json.loads(receipt.read_text())
    if (claim.get('owner') != owner or claim.get('generation') != generation
            or claim.get('output') != str(output) or claim.get('receipt') != str(receipt)
            or claim.get('state') != 'writing' or claim.get('identity') is not None):
        raise ValueError('receipt is not this pre-generation claim')
    return [*context['owner_receipt_argv'], *argv]


class RegistrationHandler(socketserver.StreamRequestHandler):
    def setup(self):
        super().setup()
        with self.server.connection_lock:
            self.server.connections.add(self.request)

    def finish(self):
        try:
            super().finish()
        finally:
            with self.server.connection_lock:
                self.server.connections.discard(self.request)

    def handle(self):
        try:
            argv = json.load(self.rfile)
            command = registration(argv, self.server.context, self.server.cwd, self.server.receipts)
            result = subprocess.run(command, cwd=self.server.cwd, capture_output=True, text=True)
            response = {'returncode': result.returncode, 'stdout': result.stdout, 'stderr': result.stderr}
        except (ValueError, OSError, KeyError, TypeError) as error:
            response = {'returncode': 1, 'stdout': '', 'stderr': 'owner registration refused: ' + str(error)}
        try:
            self.wfile.write(json.dumps(response).encode())
        except OSError:
            pass  # Docker may have exited before consuming the acknowledgement.


def client(address, argv):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.connect(address)
        connection.sendall(json.dumps(argv).encode())
        connection.shutdown(socket.SHUT_WR)
        with connection.makefile('r') as response_stream:
            response = json.load(response_stream)
    sys.stdout.write(response['stdout'])
    sys.stderr.write(response['stderr'])
    return response['returncode']


def run(docker_args):
    context, cwd, receipts = bindings(os.environ['WORKSPACE_LIFECYCLE_CONTEXT'], Path.cwd())
    # tempfile owns the private mode-0700 directory. Keep the socket path short
    # enough for the Unix domain socket platform limit, independently of cwd.
    with tempfile.TemporaryDirectory(prefix='funkot-owner-') as private:
        address = str(Path(private) / 'register.sock')
        with socketserver.ThreadingUnixStreamServer(address, RegistrationHandler) as server:
            os.chmod(address, 0o600)
            server.context, server.cwd, server.receipts = context, cwd, receipts
            server.connections = set()
            server.connection_lock = threading.Lock()
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            child_context = dict(context)
            child_context['owner_receipt_argv'] = ['/usr/bin/python3',
                str(cwd / 'tools/dev_owner_context.py'), 'register-client', address]
            docker = ['docker', 'run', '--rm', '-i', '-v', str(cwd) + ':' + str(cwd), '-w', str(cwd),
                      '-v', str(receipts) + ':' + str(receipts), '-v', private + ':' + private,
                      '-e', 'CARGO_TARGET_DIR=/work/target', '-e', 'PYTHONDONTWRITEBYTECODE=1',
                      '-e', 'WORKSPACE_LIFECYCLE_CONTEXT=' + json.dumps(child_context, separators=(',', ':')),
                      *docker_args]
            try:
                child = subprocess.Popen(docker)
                previous = {}
                for sig in (signal.SIGINT, signal.SIGTERM):
                    previous[sig] = signal.signal(sig, lambda received, _frame: child.send_signal(received))
                try:
                    status = child.wait()
                finally:
                    for sig, handler in previous.items():
                        signal.signal(sig, handler)
                return status if status >= 0 else 128 - status
            finally:
                # Acknowledgements stay available until Docker has exited.
                server.shutdown()
                with server.connection_lock:
                    connections = list(server.connections)
                for connection in connections:
                    try:
                        connection.shutdown(socket.SHUT_RDWR)
                    except OSError:
                        pass
                thread.join()


def main(argv):
    try:
        if argv[:1] == ['run']:
            return run(argv[1:])
        if argv[:1] == ['register-client'] and len(argv) > 2:
            return client(argv[1], argv[2:])
        raise ValueError('expected run or register-client')
    except (ValueError, OSError, KeyError, TypeError, subprocess.CalledProcessError) as error:
        print('managed dev.sh owner bridge refused: ' + str(error), file=sys.stderr)
        return 1


if __name__ == '__main__':
    raise SystemExit(main(sys.argv[1:]))
