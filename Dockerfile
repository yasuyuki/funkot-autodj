# Development / build container for funkot-autodj.
#
# Build the image:
#   docker build -t funkot-autodj-dev .
# Run any cargo command inside it (cargo caches persist in named volumes):
#   ./dev.sh cargo build --workspace
#   ./dev.sh cargo test --workspace
#   ./dev.sh cargo run -p funkot-cli -- -l playlist.txt --render out.wav
FROM rust:1.93-slim-trixie

RUN apt-get update && apt-get install -y --no-install-recommends \
    # C++ toolchain for the signalsmith-stretch native code
    g++ \
    # libclang for bindgen (signalsmith-stretch FFI bindings)
    libclang-dev \
    # ALSA headers + pkg-config for cpal on Linux
    pkg-config \
    libasound2-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /work
