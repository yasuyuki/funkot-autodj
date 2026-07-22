#!/bin/sh
# Run a command inside the dev container with the repo mounted at /work.
# Cargo registry and target dir live in named volumes so rebuilds are fast.
#
# Usage: ./dev.sh cargo build --workspace
# Optional: DEV_BIND_SRC=/host/path DEV_BIND_DST=/host/path (default: same as src)
set -eu
cd "$(dirname "$0")"

IMAGE=funkot-autodj-dev

if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    docker build -t "$IMAGE" .
fi

# The container runs as root; hand ownership of anything it wrote in the
# mounted workspace (Cargo.lock, testdata, ...) back to the invoking user.
#
# DEV_BIND_*: same-path (or remapped) bind for music dirs outside the repo
# (work.sh playlists with /mnt/c/... paths). Quoted -v keeps spaces safe.
if [ -n "${DEV_BIND_SRC:-}" ]; then
    exec docker run --rm -i \
        -v "$PWD":/work \
        -v "$DEV_BIND_SRC:${DEV_BIND_DST:-$DEV_BIND_SRC}:ro" \
        -v funkot-cargo-registry:/usr/local/cargo/registry \
        -v funkot-target:/work/target \
        -e CARGO_TERM_COLOR=never \
        -e HOST_UID="$(id -u)" \
        -e HOST_GID="$(id -g)" \
        "$IMAGE" sh -c '"$@"; status=$?; chown -R "$HOST_UID:$HOST_GID" /work 2>/dev/null || true; exit $status' -- "$@"
fi

exec docker run --rm -i \
    -v "$PWD":/work \
    -v funkot-cargo-registry:/usr/local/cargo/registry \
    -v funkot-target:/work/target \
    -e CARGO_TERM_COLOR=never \
    -e HOST_UID="$(id -u)" \
    -e HOST_GID="$(id -g)" \
    "$IMAGE" sh -c '"$@"; status=$?; chown -R "$HOST_UID:$HOST_GID" /work 2>/dev/null || true; exit $status' -- "$@"
