#!/usr/bin/env python3
"""Run one dev.sh command so Cargo cannot reuse a build of different sources.

Cargo treats a unit as fresh when no source is newer than the unit's dep-info.
Files replaced in place by older ones (an archive extracted over a checkout, a
mutation reverted with `cp -p`) keep the previous build, and its test binary
runs against the new tree. After each command this records the identity of
every checkout file rustc read. Before the next one, a recorded file whose
identity changed while its mtime stayed at or before that record makes every
unit that read it rebuild. A newer mtime is left to Cargo.
"""
import fcntl
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time

RECORD = '.funkot-dev-sources.json'


def identity(path):
    try:
        st = os.stat(path)
    except FileNotFoundError:
        return None
    return [st.st_ino, st.st_size, st.st_mtime_ns]


def checkout_units(target, root):
    """Map each rustc dep-info under target to the checkout files it read."""
    units = {}
    for dep in [*target.glob('*/deps/*.d'), *target.glob('*/*/deps/*.d')]:
        sources = set()
        for line in dep.read_text(errors='replace').splitlines():
            if line.startswith('#') or not line.endswith(':'):
                continue
            path = Path(root, line[:-1].replace('\\ ', ' '))
            if root in path.parents and target not in path.parents:
                sources.add(str(path))
        if sources:
            units[dep] = sources
    return units


def invalidate(deps):
    """Delete the Cargo fingerprints of these units under the build lock."""
    by_profile = {}
    for dep in deps:
        by_profile.setdefault(dep.parent.parent, []).append(dep)
    for profile, units in by_profile.items():
        with open(profile / '.cargo-lock', 'a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            for dep in units:
                unit_hash = dep.stem.rpartition('-')[2]
                for fingerprint in (profile / '.fingerprint').glob('*-' + unit_hash):
                    shutil.rmtree(fingerprint, ignore_errors=True)


def check(target, root):
    try:
        record = json.loads((target / RECORD).read_text())
    except (FileNotFoundError, ValueError):
        record = None
    units = checkout_units(target, root)
    seen = {}
    if record is None:
        stale = list(units)
    else:
        changed = set()
        for path, built in record['files'].items():
            seen[path] = now = identity(path)
            if now is not None and now != built and now[2] <= record['time_ns']:
                changed.add(path)
        # A unit written after the record has no recorded sources: an
        # interrupted run built it.
        stale = [dep for dep, sources in units.items()
                 if sources & changed or dep.stat().st_mtime_ns > record['time_ns']]
    if stale:
        print(f'dev.sh: rebuilding {len(stale)} build unit(s) whose checkout sources '
              'changed without a newer mtime', file=sys.stderr)
        invalidate(stale)
    return seen


def record(target, root, seen):
    files = {}
    for sources in checkout_units(target, root).values():
        for path in sources:
            built = seen.get(path) or identity(path)
            if built is not None:
                files[path] = built
    temporary = target / (RECORD + f'.{os.getpid()}')
    temporary.write_text(json.dumps({'time_ns': time.time_ns(), 'files': files}))
    os.replace(temporary, target / RECORD)


def main(argv):
    root = Path.cwd()
    target = Path(root, os.environ.get('CARGO_TARGET_DIR', 'target'))
    target.mkdir(parents=True, exist_ok=True)
    seen = check(target, root)
    status = subprocess.call(argv) if argv else 0
    record(target, root, seen)
    return status if status >= 0 else 128 - status


if __name__ == '__main__':
    raise SystemExit(main(sys.argv[1:]))
