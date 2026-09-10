#!/bin/sh
# click_phase_diag over every track in testdata/file_list.txt (absolute paths
# pass through, bare names are testdata/-relative). Companion to _survey.sh.
set -eu
cd /work
IFS='
'
set --
while read -r line; do
    [ -n "$line" ] || continue
    case "$line" in
        /*) set -- "$@" "$line" ;;
        *)  set -- "$@" "testdata/$line" ;;
    esac
done < testdata/file_list.txt
exec cargo run -q -p funkot-cli --release --example offbeat_diag -- \
    --cache-dir funkot-cache "$@"
