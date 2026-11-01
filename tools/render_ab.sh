#!/bin/sh
set -eu
cd /work
IFS='
'
set --
while read -r line; do
    case "$line" in
        /*) set -- "$@" "$line" ;;
    esac
done < testdata/file_list.txt
echo "=== tracks: $# ==="
for s in intro outro; do
    echo "=== side $s ==="
    cargo run -q -p funkot-cli --release --example phase_ab -- \
        --cache-dir funkot-cache --side "$s" --bars 16 \
        --out testdata/phase_ab "$@"
done
