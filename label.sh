#!/bin/sh
# Build funkot-autodj on the host and launch interactive section labeling
# (`--label-sections`) against the checked-in testdata fixtures.
#
# Usage:
#   ./label.sh                  # host build, then launch labeling
#   ./label.sh --no-build       # skip the build, run the existing binary
#   ./label.sh --rate 1.2       # extra args are passed through to funkot-autodj
#   ./label.sh -h | --help
#
# All paths (testdata/file_list.txt, testdata/labels.tsv, funkot-cache,
# target-host) are resolved relative to this script's own location, not the
# current directory, so it behaves the same run from anywhere.
set -eu
cd "$(dirname "$0")"

usage() {
    echo "usage: ./label.sh [--no-build] [funkot-autodj args...]"
    echo
    echo "  --no-build   skip the host build; run the existing target-host binary"
    echo "  -h, --help   print this message"
    echo "  [args...]    passed through to funkot-autodj as-is (e.g. --rate 1.2)"
    echo
    echo "Builds funkot-autodj on the host (CARGO_TARGET_DIR=target-host) and runs:"
    echo "  ./target-host/release/funkot-autodj --label-sections \\"
    echo "    -l testdata/file_list.txt --labels testdata/labels.tsv \\"
    echo "    --cache-dir funkot-cache"
}

BUILD=1
# Pass-through args are re-collected into "$@" rather than a string, so an
# argument containing a space (a path, say) survives instead of being word
# -split at the exec below. "$@" is expanded once, before the loop starts,
# so rewriting it with `set --` from inside the loop is safe.
collected=0
for arg in "$@"; do
    case "$arg" in
        -h|--help)
            usage
            exit 0
            ;;
        --no-build)
            BUILD=0
            ;;
        *)
            if [ "$collected" -eq 0 ]; then
                set -- "$arg"
                collected=1
            else
                set -- "$@" "$arg"
            fi
            ;;
    esac
done
if [ "$collected" -eq 0 ]; then
    set --
fi

if [ ! -f testdata/file_list.txt ]; then
    echo "error: testdata/file_list.txt not found (see docs/labeling.md for how to build it)" >&2
    exit 1
fi

if [ "$BUILD" -eq 1 ]; then
    CARGO_TARGET_DIR=target-host \
    PKG_CONFIG_PATH=/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/share/pkgconfig \
        cargo build -p funkot-cli --release
fi

exec ./target-host/release/funkot-autodj --label-sections \
    -l testdata/file_list.txt --labels testdata/labels.tsv --cache-dir funkot-cache \
    "$@"
