#!/bin/sh
# Cross-build release binaries for non-Linux targets (Docker + mingw).
#
# Usage:
#   ./cross-build.sh                 # Windows x64
#   ./cross-build.sh windows         # same
#   ./cross-build.sh --clean         # wipe target-cross + dist, then build
#   ./cross-build.sh windows --clean # same
#
# Output: dist/windows-x64/funkot-autodj.exe (+ MinGW runtime DLLs)
set -eu
cd "$(dirname "$0")"

CLEAN=0
TARGET_ARG=windows
for arg in "$@"; do
    case "$arg" in
        --clean|clean) CLEAN=1 ;;
        *) TARGET_ARG=$arg ;;
    esac
done

IMAGE=funkot-autodj-cross
TRIPLE=x86_64-pc-windows-gnu
DIST_DIR=dist/windows-x64
CARGO_TARGET_DIR=target-cross

case "$TARGET_ARG" in
    windows|win|x86_64-pc-windows-gnu) ;;
    *)
        echo "unsupported target: $TARGET_ARG (only windows for now)" >&2
        exit 1
        ;;
esac

if [ "$CLEAN" = 1 ]; then
    rm -rf "$CARGO_TARGET_DIR" "$DIST_DIR"
fi

if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    docker build -f Dockerfile.cross -t "$IMAGE" .
fi

mkdir -p "$DIST_DIR" "$CARGO_TARGET_DIR"

docker run --rm -i \
    -v "$PWD":/work \
    -v funkot-cargo-registry:/usr/local/cargo/registry \
    -e CARGO_TERM_COLOR=never \
    -e CARGO_TARGET_DIR="/work/$CARGO_TARGET_DIR" \
    -e HOST_UID="$(id -u)" \
    -e HOST_GID="$(id -g)" \
    "$IMAGE" sh -c '
        set -eu
        # shellcheck disable=SC1091
        . /etc/funkot-bindgen.env
        export BINDGEN_EXTRA_CLANG_ARGS_x86_64_pc_windows_gnu
        cargo build -p funkot-cli --release --target '"$TRIPLE"'
        mkdir -p /work/'"$DIST_DIR"'
        cp "/work/'"$CARGO_TARGET_DIR"'/'"$TRIPLE"'/release/funkot-autodj.exe" \
            /work/'"$DIST_DIR"'/funkot-autodj.exe
        # signalsmith pulls in libstdc++; ship MinGW runtimes next to the exe
        # (Windows will not find them on PATH when launched from a WSL UNC share).
        for name in libstdc++-6.dll libgcc_s_seh-1.dll libwinpthread-1.dll; do
            src="$(x86_64-w64-mingw32-g++ -print-file-name="$name")"
            case "$src" in
                */*) cp "$src" /work/'"$DIST_DIR"'/"$name" ;;
                *) echo "missing mingw dll: $name (got: $src)" >&2; exit 1 ;;
            esac
        done
        chown -R "$HOST_UID:$HOST_GID" /work/'"$CARGO_TARGET_DIR"' /work/dist 2>/dev/null || true
    '

echo "built $DIST_DIR/funkot-autodj.exe"
