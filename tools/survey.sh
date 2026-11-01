#!/bin/sh
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
exec cargo run -q -p funkot-cli --release --example bar_phase_diag -- \
    --cache-dir funkot-cache "$@"
