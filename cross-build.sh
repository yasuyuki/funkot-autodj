#!/bin/sh
# Cross-build release artifacts for non-Linux targets (Docker + mingw / Android NDK).
#
# Usage:
#   ./cross-build.sh                 # Windows x64
#   ./cross-build.sh windows         # same
#   ./cross-build.sh android         # Android arm64 (funkot-core C-ABI SDK)
#   ./cross-build.sh --clean         # wipe target-cross + dist, then build
#   ./cross-build.sh windows --clean # same
#
# Output:
#   windows: dist/windows-x64/funkot-autodj.exe (+ MinGW runtime DLLs)
#   android: dist/android-arm64/{libfunkot_core.so,libfunkot_core.a,
#            libc++_shared.so,include/funkot.h}
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

CARGO_TARGET_DIR=target-cross

case "$TARGET_ARG" in
    windows|win|x86_64-pc-windows-gnu)
        TARGET_ARG=windows
        IMAGE=funkot-autodj-cross
        DOCKERFILE=Dockerfile.cross
        TRIPLE=x86_64-pc-windows-gnu
        DIST_DIR=dist/windows-x64
        ;;
    android|aarch64-linux-android)
        TARGET_ARG=android
        IMAGE=funkot-autodj-android
        DOCKERFILE=Dockerfile.android
        TRIPLE=aarch64-linux-android
        DIST_DIR=dist/android-arm64
        ;;
    *)
        echo "unsupported target: $TARGET_ARG (windows, android)" >&2
        exit 1
        ;;
esac

if [ "$CLEAN" = 1 ]; then
    rm -rf "$CARGO_TARGET_DIR" "$DIST_DIR"
fi

if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    docker build -f "$DOCKERFILE" -t "$IMAGE" .
fi

mkdir -p "$DIST_DIR" "$CARGO_TARGET_DIR"

if [ "$TARGET_ARG" = windows ]; then
    BUILD_SCRIPT='
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
    BUILT="$DIST_DIR/funkot-autodj.exe"
else
    # funkot-cli has no Android host (cpal + terminal keys); only the core SDK.
    # The cdylib links libc++_shared.so, so ship it alongside — a static libc++
    # is not an option once an app loads more than one .so.
    BUILD_SCRIPT='
        set -eu
        cargo build -p funkot-core --release --target '"$TRIPLE"'
        mkdir -p /work/'"$DIST_DIR"'/include
        OUT="/work/'"$CARGO_TARGET_DIR"'/'"$TRIPLE"'/release"
        cp "$OUT/libfunkot_core.so" "$OUT/libfunkot_core.a" /work/'"$DIST_DIR"'/
        cp "$NDK_SYSROOT/usr/lib/aarch64-linux-android/libc++_shared.so" \
            /work/'"$DIST_DIR"'/
        cp /work/include/funkot.h /work/'"$DIST_DIR"'/include/
        chown -R "$HOST_UID:$HOST_GID" /work/'"$CARGO_TARGET_DIR"' /work/dist 2>/dev/null || true
    '
    BUILT="$DIST_DIR/libfunkot_core.so"
fi

docker run --rm -i \
    -v "$PWD":/work \
    -v funkot-cargo-registry:/usr/local/cargo/registry \
    -e CARGO_TERM_COLOR=never \
    -e CARGO_TARGET_DIR="/work/$CARGO_TARGET_DIR" \
    -e HOST_UID="$(id -u)" \
    -e HOST_GID="$(id -g)" \
    "$IMAGE" sh -c "$BUILD_SCRIPT"

echo "built $BUILT"
