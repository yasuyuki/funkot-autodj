#!/bin/sh
# Build funkot-autodj on the host and launch interactive section labeling
# (`--label-sections`) against the checked-in testdata fixtures.
#
# Usage:
#   ./label.sh                  # host build, then launch labeling
#   ./label.sh --no-build       # skip the build, run the existing binary
#   ./label.sh --list FILE      # label FILE's tracks instead of the full list
#   ./label.sh --survey         # outro-grid survey instead of --label-sections
#   ./label.sh --rate 1.2       # extra args are passed through to funkot-autodj
#   ./label.sh -h | --help
#
# The default paths (testdata/file_list.txt, testdata/labels.tsv,
# testdata/survey.tsv, funkot-cache, target-host) are resolved relative to
# this script's own location, not the current directory, so it behaves the
# same run from anywhere. A --list given on the command line is the
# exception: a relative one resolves against the directory the script was
# invoked from, which is what a caller typing a path expects.
set -eu
invoked_from=$PWD
cd "$(dirname "$0")"

usage() {
    echo "usage: ./label.sh [--no-build] [--list FILE] [--survey] [funkot-autodj args...]"
    echo
    echo "  --no-build   skip the host build; run the existing target-host binary"
    echo "  --list FILE  playlist to label (default testdata/file_list.txt)"
    echo "  --survey     run the outro-grid survey instead of --label-sections"
    echo "  -h, --help   print this message"
    echo "  [args...]    passed through to funkot-autodj as-is (e.g. --rate 1.2)"
    echo
    echo "Builds funkot-autodj on the host (CARGO_TARGET_DIR=target-host) and runs:"
    echo "  ./target-host/release/funkot-autodj --label-sections \\"
    echo "    -l <list> --labels testdata/labels.tsv --cache-dir funkot-cache"
    echo "or, with --survey:"
    echo "  ./target-host/release/funkot-autodj --survey \\"
    echo "    -l <list> --survey-out testdata/survey.tsv --cache-dir funkot-cache"
}

BUILD=1
LIST=testdata/file_list.txt
list_given=0
SURVEY=0
# --list takes a value, so this consumes "$@" from the front instead of
# iterating it with `for`, which cannot look ahead to the next argument.
# Unrecognised args are pushed onto the end of "$@"; `remaining` counts only
# the original arguments, so the front of "$@" is always still an original
# while the loop runs, and once they are used up "$@" holds exactly the
# pass-through args in order. Rebuilding "$@" rather than a string is what
# lets an argument containing a space (a path, say) survive to the exec below.
remaining=$#
while [ "$remaining" -gt 0 ]; do
    arg=$1
    shift
    remaining=$((remaining - 1))
    case "$arg" in
        -h|--help)
            usage
            exit 0
            ;;
        --no-build)
            BUILD=0
            ;;
        --survey)
            # Swallowed rather than passed through: the exec below adds its
            # own --survey plus --survey-out, mirroring how --label-sections
            # itself is never something the caller types.
            SURVEY=1
            ;;
        -l|--list)
            # Swallowed rather than passed through: funkot-autodj rejects a
            # repeated --list, so letting this reach the exec below would
            # collide with the one it already passes.
            if [ "$remaining" -eq 0 ]; then
                echo "error: $arg needs a FILE argument" >&2
                exit 1
            fi
            LIST=$1
            list_given=1
            shift
            remaining=$((remaining - 1))
            ;;
        *)
            set -- "$@" "$arg"
            ;;
    esac
done

# A caller-supplied relative --list means "relative to where I typed it",
# not to the script's directory that the cd above moved us to. The default
# is already script-relative and must not be rewritten.
if [ "$list_given" -eq 1 ]; then
    case "$LIST" in
        /*) ;;
        *) LIST="$invoked_from/$LIST" ;;
    esac
fi

if [ ! -f "$LIST" ]; then
    echo "error: playlist not found: $LIST" >&2
    echo "       (see docs/labeling.md for how to build one)" >&2
    exit 1
fi

if [ "$BUILD" -eq 1 ]; then
    CARGO_TARGET_DIR=target-host \
    PKG_CONFIG_PATH=/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/share/pkgconfig \
        cargo build -p funkot-cli --release
fi

if [ "$SURVEY" -eq 1 ]; then
    exec ./target-host/release/funkot-autodj --survey \
        -l "$LIST" --survey-out testdata/survey.tsv --cache-dir funkot-cache \
        "$@"
fi

exec ./target-host/release/funkot-autodj --label-sections \
    -l "$LIST" --labels testdata/labels.tsv --cache-dir funkot-cache \
    "$@"
