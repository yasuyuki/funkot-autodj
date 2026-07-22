#!/bin/sh
# Run a command inside the dev container with the repo mounted at /work.
# Cargo registry and target dir live in named volumes so rebuilds are fast.
#
# Usage: ./dev.sh cargo build --workspace
set -eu
cd "$(dirname "$0")"

IMAGE=funkot-autodj-dev

if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    docker build -t "$IMAGE" .
fi

# The container runs as root; hand ownership of anything it wrote in the
# mounted workspace (Cargo.lock, testdata, ...) back to the invoking user.
exec docker run --rm -i \
    -v "$PWD":/work \
    -v funkot-cargo-registry:/usr/local/cargo/registry \
    -v funkot-target:/work/target \
    -e CARGO_TERM_COLOR=never \
    -e HOST_UID="$(id -u)" \
    -e HOST_GID="$(id -g)" \
    "$IMAGE" sh -c '"$@"; status=$?; chown -R "$HOST_UID:$HOST_GID" /work 2>/dev/null || true; exit $status' -- "$@"
