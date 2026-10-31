#!/usr/bin/env bash
set -Eeuo pipefail
root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
image=funkot-transition-kani:0.68.0
if [[ ${1:-} == --setup ]]; then
    exec docker build -t "$image" -f "$root/verification/Dockerfile" "$root/verification"
fi
if (( $# > 1 )); then echo "usage: $0 [--setup | evidence-directory]" >&2; exit 2; fi
out=${1:-$(mktemp -d "${TMPDIR:-/tmp}/transition-geometry.XXXXXXXX")}
mkdir -p -- "$out"
out=$(cd -- "$out" && pwd)
echo "Evidence: $out"
reason="verification did not complete"
write_failure() {
    python3 - "$out/summary.json" "$reason" <<'JSON'
import json
from pathlib import Path
import sys
path = Path(sys.argv[1])
summary = {} if sys.argv[2] == "verification did not complete" else json.loads(path.read_text())
summary.update(status="FAIL", runner_error=sys.argv[2])
if summary.get("error") in (None, "verification did not complete"):
    summary["error"] = sys.argv[2]
Path(sys.argv[1]).write_text(json.dumps(summary, indent=2) + "\n")
if sys.argv[2] != "verification did not complete":
    print(json.dumps(summary), file=sys.stderr)
JSON
}
trap 'write_failure; exit 1' ERR
write_failure
reason="cannot resolve source HEAD commit"
revision=$(git -C "$root" rev-parse --verify 'HEAD^{commit}')
reason="invalid source revision"
[[ $revision =~ ^([0-9a-f]{40}|[0-9a-f]{64})$ ]]
reason="cannot inspect source status"
dirty=$(git -C "$root" status --porcelain --untracked-files=normal)
reason="source checkout is dirty; commit or remove owned changes before verification"
[[ -z $dirty ]]
reason="source archive or verifier failed; inspect kani.log when present"
# The archive binds every executed source file to the resolved commit, even if
# another process changes the checkout after the clean-state check.
git -C "$root" archive --format=tar "$revision" |
    docker run --rm -i --memory=8g --memory-swap=8g \
        -e SOURCE_REVISION="$revision" -v "$out:/results" "$image" \
        sh -c 'tar -xf - && python3 verification/check.py'
