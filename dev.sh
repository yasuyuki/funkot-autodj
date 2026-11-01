#!/bin/sh
set -eu
cd "$(dirname "$0")"

if ! command -v docker >/dev/null 2>&1; then
    echo "Docker Engine is required but \`docker\` was not found on PATH." >&2
    echo "Install Docker Engine and ensure \`docker\` works for your user." >&2
    echo "See docs/development-setup.md" >&2
    exit 127
fi

IMAGE=funkot-autodj-dev

DOCKERFILE_SHA=$(sha256sum Dockerfile | cut -d ' ' -f 1)
IMAGE_DOCKERFILE_SHA=$(docker image inspect --format '{{ index .Config.Labels "org.funkot.dev-dockerfile" }}' "$IMAGE" 2>/dev/null || true)
if [ "$IMAGE_DOCKERFILE_SHA" != "$DOCKERFILE_SHA" ]; then
    docker build --label "org.funkot.dev-dockerfile=$DOCKERFILE_SHA" -t "$IMAGE" . >&2
fi

if [ -z "${DEV_JOBS:-}" ]; then
    if command -v nproc >/dev/null 2>&1; then
        DEV_JOBS=$(nproc)
    elif command -v sysctl >/dev/null 2>&1; then
        DEV_JOBS=$(sysctl -n hw.ncpu 2>/dev/null || echo 1)
    else
        DEV_JOBS=1
    fi
fi
: "${CARGO_BUILD_JOBS:=$DEV_JOBS}"
: "${RUST_TEST_THREADS:=$DEV_JOBS}"
: "${CARGO_PROFILE_RELEASE_LTO:=false}"
: "${CARGO_INCREMENTAL:=1}"

# Only do this under rootful Docker. Under rootless Docker, container UID 0
# already *is* the invoking host user, and any other container UID (such as
# $HOST_UID) is remapped through /etc/subuid to a disjoint high host UID
# range -- chown-ing to "$HOST_UID:$HOST_GID" there does not restore the
# invoking user's ownership, it reassigns everything to that subuid-mapped
# id and locks the invoking user out instead.
if docker info --format '{{range .SecurityOptions}}{{.}}{{"\n"}}{{end}}' 2>/dev/null \
    | grep -qx 'name=rootless'; then
    CHOWN_WORK=':'
else
    CHOWN_WORK='find /work -mindepth 1 -maxdepth 1 ! -name target -exec chown -R "$HOST_UID:$HOST_GID" {} + 2>/dev/null || true'
fi

# Cargo's mtime freshness cannot distinguish restored or metadata-preserving
# source edits. The content guard invalidates affected local fingerprints.
SOURCE_PATH=$(pwd -P)
TARGET_VOLUME=${DEV_TARGET_PREFIX:-funkot-target}
docker volume create "$TARGET_VOLUME" >/dev/null
SOURCE_REVISION=
if [ "$(git rev-parse --show-toplevel 2>/dev/null)" = "$SOURCE_PATH" ] \
    && [ -z "$(git status --porcelain)" ]; then
    SOURCE_REVISION=$(git rev-parse HEAD)
fi
set -- "$IMAGE" sh -c 'python3 tools/dev_source_guard.py "$@"; status=$?; '"$CHOWN_WORK"'; exit $status' -- "$@"
set -- -v "$PWD":/work \
    -v funkot-cargo-registry:/usr/local/cargo/registry \
    -v "$TARGET_VOLUME":/work/target \
    -e CARGO_TARGET_DIR=/work/target \
    -e CARGO_TERM_COLOR=never \
    -e FUNKOT_TESTDATA_DIR="${FUNKOT_TESTDATA_DIR:-}" \
    -e FUNKOT_SOURCE_REVISION="$SOURCE_REVISION" \
    -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
    -e CARGO_BUILD_JOBS="$CARGO_BUILD_JOBS" \
    -e RUST_TEST_THREADS="$RUST_TEST_THREADS" \
    -e CARGO_PROFILE_RELEASE_LTO="$CARGO_PROFILE_RELEASE_LTO" \
    -e CARGO_INCREMENTAL="$CARGO_INCREMENTAL" "$@"
if [ -n "${DEV_BIND_SRC:-}" ]; then
    set -- -v "$DEV_BIND_SRC:${DEV_BIND_DST:-$DEV_BIND_SRC}:ro" "$@"
fi
if [ -n "${WORKSPACE_LIFECYCLE_CONTEXT:-}" ]; then
    exec python3 tools/dev_owner_context.py run "$@"
fi
exec docker run --rm -i "$@"
