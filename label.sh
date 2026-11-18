#!/bin/sh
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
            SURVEY=1
            ;;
        -l|--list)
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
