FROM rust:1.93-slim-trixie

RUN apt-get update && apt-get install -y --no-install-recommends \
    g++ \
    libclang-dev \
    git \
    pkg-config \
    python3 \
    libasound2-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /work
