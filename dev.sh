#!/bin/sh
# Run a command inside the dev container with the repo mounted at /work.
# Cargo registry and target dir live in named volumes so rebuilds are fast.
#
# Usage: ./dev.sh cargo build --workspace
# Optional: DEV_BIND_SRC=/host/path DEV_BIND_DST=/host/path (default: same as src)
#
# FUNKOT_TESTDATA_DIR is forwarded when set (see funkot_core::testdata). Pointing
# it outside the repo needs DEV_BIND_SRC too, or the path won't exist in here.
#
# Local CI speed (dev.sh only; GitHub Actions is unchanged):
#   DEV_JOBS             — cargo/rustc/test parallelism (default: nproc / hw.ncpu)
#   CARGO_BUILD_JOBS     — override build jobs (default: DEV_JOBS)
#   RUST_TEST_THREADS    — override test threads (default: DEV_JOBS)
#   CARGO_PROFILE_RELEASE_LTO — default false (matches GH test job; thin LTO is slow)
#   CARGO_INCREMENTAL    — default 1 (faster local release rebuilds)
set -eu
cd "$(dirname "$0")"

if ! command -v docker >/dev/null 2>&1; then
    echo "Docker Engine is required but \`docker\` was not found on PATH." >&2
    echo "Install Docker Engine and ensure \`docker\` works for your user." >&2
    echo "See docs/development-setup.md" >&2
    exit 127
fi

IMAGE=funkot-autodj-dev

# Refresh the image when its declared dependencies change, including an old
# image built before this label existed.
DOCKERFILE_SHA=$(sha256sum Dockerfile | cut -d ' ' -f 1)
IMAGE_DOCKERFILE_SHA=$(docker image inspect --format '{{ index .Config.Labels "org.funkot.dev-dockerfile" }}' "$IMAGE" 2>/dev/null || true)
if [ "$IMAGE_DOCKERFILE_SHA" != "$DOCKERFILE_SHA" ]; then
    docker build --label "org.funkot.dev-dockerfile=$DOCKERFILE_SHA" -t "$IMAGE" . >&2
fi

# Host CPU count → container cargo/test parallelism.
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
# GH test job sets LTO=false; keep official release artifacts on Actions/cross-build.
: "${CARGO_PROFILE_RELEASE_LTO:=false}"
: "${CARGO_INCREMENTAL:=1}"

# Skip named volume /work/target (multi-GB); only fix bind-mount ownership.
#
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

# The container runs as root; hand ownership of anything it wrote in the
# mounted workspace (Cargo.lock, testdata, ...) back to the invoking user.
#
# DEV_BIND_*: same-path (or remapped) bind for music dirs outside the repo
# (work.sh playlists with /mnt/c/... paths). Quoted -v keeps spaces safe.
# Preserve the command as argv throughout: no shell evaluation of context or
# user arguments. Build Docker options before the image/command boundary.
set -- "$IMAGE" sh -c '"$@"; status=$?; '"$CHOWN_WORK"'; exit $status' -- "$@"
set -- -v "$PWD":/work \
    -v funkot-cargo-registry:/usr/local/cargo/registry \
    -v funkot-target:/work/target \
    -e CARGO_TERM_COLOR=never \
    -e FUNKOT_TESTDATA_DIR="${FUNKOT_TESTDATA_DIR:-}" \
    -e FUNKOT_SOURCE_REVISION="$(git rev-parse HEAD)" \
    -e HOST_UID="$(id -u)" -e HOST_GID="$(id -g)" \
    -e CARGO_BUILD_JOBS="$CARGO_BUILD_JOBS" \
    -e RUST_TEST_THREADS="$RUST_TEST_THREADS" \
    -e CARGO_PROFILE_RELEASE_LTO="$CARGO_PROFILE_RELEASE_LTO" \
    -e CARGO_INCREMENTAL="$CARGO_INCREMENTAL" "$@"
if [ -n "${DEV_BIND_SRC:-}" ]; then
    set -- -v "$DEV_BIND_SRC:${DEV_BIND_DST:-$DEV_BIND_SRC}:ro" "$@"
fi
if [ -n "${WORKSPACE_LIFECYCLE_CONTEXT:-}" ]; then
    # Registration runs on the host with the supervisor's existing lease.
    # The container receives only a fixed run-scoped registration socket,
    # its task checkout, and its external owner receipt directory.
    exec python3 tools/dev_owner_context.py run "$@"
fi
exec docker run --rm -i "$@"
