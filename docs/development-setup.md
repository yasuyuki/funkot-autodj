# Development setup

External-contributor guide for a first successful `./dev.sh` run.
These steps were verified on Ubuntu 24.04 under WSL2 with systemd.

Day-to-day build/test examples and debug helpers stay in [README.md § Development](../README.md#development).

## Prerequisites

- **Docker Engine** on the host (CLI + daemon). Host Rust is not required for the usual Docker workflow; `./dev.sh` runs cargo inside the `funkot-autodj-dev` image.

## Install Docker

Install official Docker Engine and confirm `docker` works for your user. Official docs: [Install Docker Engine](https://docs.docker.com/engine/install/).

A fuller Ubuntu/WSL2 walkthrough (including `systemd=true` under WSL2) is in the player guide: [funkot-player `docs/development-setup.md`](https://github.com/yasuyuki/funkot-player/blob/main/docs/development-setup.md#install-docker). Short summary:

1. Install Docker CE from Docker’s apt repository (not only a stub `docker` package).
2. `sudo systemctl enable --now docker`
3. `sudo usermod -aG docker "$USER"`, then log out/in (or `newgrp docker`)
4. `docker run --rm hello-world`

On WSL2, `/etc/wsl.conf` must have `systemd=true` so the daemon can start.

### Example: Ubuntu 24.04 / WSL2 + systemd

```sh
sudo apt-get update
sudo apt-get install -y ca-certificates curl
sudo install -m 0755 -d /etc/apt/keyrings
sudo curl -fsSL https://download.docker.com/linux/ubuntu/gpg -o /etc/apt/keyrings/docker.asc
sudo chmod a+r /etc/apt/keyrings/docker.asc
echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/ubuntu $(. /etc/os-release && echo "$VERSION_CODENAME") stable" | sudo tee /etc/apt/sources.list.d/docker.list > /dev/null
sudo apt-get update
sudo apt-get install -y docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
sudo systemctl enable --now docker
sudo usermod -aG docker "$USER"
# log out/in or: newgrp docker
docker run --rm hello-world
```

## First-time verification

From the repo root:

```sh
RUST_TEST_THREADS=1 ./dev.sh cargo test --workspace --release
```

Notes:

- The first run builds the `funkot-autodj-dev` image.
- CLI transition-clip tests use the existing `--jobs 2` prepare-first path. At accelerated render speed, the streaming loader can legitimately reach the end before the next track is ready; serial test scheduling alone did not make clip assertions deterministic.
- Without a real audio library, tests that need external tracks may **skip** and the suite can still finish green.

## Host cargo exception

Checks that need a live audio device use host `cargo` (outside Docker). See [README.md § Development](../README.md#development) / Debug helpers. On Linux that path needs `libasound2-dev` and `pkg-config`. Builds and tests otherwise stay in `./dev.sh`. Dependency inspection below needs no native build dependencies.

## Dependency maintenance

Weekly proposals cover Cargo, Actions and the root Dockerfiles via
[Dependabot](../.github/dependabot.yml). Review updates separately; 0.x minor
updates can break compatibility. Docker proposals do not update the Android
NDK/API constants or CI's LLVM installer. Rust changes must keep all Dockerfiles
and CI aligned; the dependency-policy job checks this before merge.

For candidates without compiling or building a container, use an installed
Cargo matching CI's `RUST_VERSION`, from the repository root:

```sh
cargo update --dry-run
```

This reports resolution within current manifest constraints, not every newer
major version. To adopt a reviewed candidate, use
`cargo update -p <package> --precise <version>`, inspect the lockfile diff, and
run the existing tests with `--locked` (plus Android validation for dependency
changes, as required in [AGENTS.md](../AGENTS.md)).

The CI workflow's manual **Run workflow** defaults to **audit_only**, running
`cargo deny --locked check advisories licenses sources` without tests or
packaging. Scheduled audits also run when there are no recent commits.
An audit or database-fetch failure fails the job; inspect its log before
changing dependencies or policy.

## Optional real-audio tests

Point optional real-track tests at a music directory (placeholder path):

```sh
export FUNKOT_TESTDATA_DIR=/path/to/music
export DEV_BIND_SRC=/path/to/music
RUST_TEST_THREADS=1 ./dev.sh cargo test --workspace --release
```

`dev.sh` forwards `FUNKOT_TESTDATA_DIR` into the container. Paths outside the repo need `DEV_BIND_SRC` (and optional `DEV_BIND_DST`) so the bind mount exists inside the container. If tracks are missing, those tests skip rather than fail.

## Labels / private evaluation data

Hand-made labels and private evaluation artifacts are **not** required for a public clone or the smoke command above. Classification and restore notes live in [`docs/local-data.md`](https://github.com/yasuyuki/funkot-autodj/blob/master/docs/local-data.md) on the engine’s default branch (that file may be absent on older player-facing checkouts).

## Common failures

| Symptom | What to do |
|---|---|
| `docker: command not found` / `dev.sh` exit 127 | Install Docker Engine; see [Install Docker](#install-docker). |
| `permission denied` on the Docker socket | Add your user to the `docker` group, then log out/in (or `newgrp docker`). |
| No transition clip with accelerated streaming render | Use the existing `--jobs` / `--ci-fast` prepare-first mode when all transitions are required. |
| Image build fails for disk space | Free space and retry `./dev.sh ...`. |
| crates.io timeout during `cargo` | Re-run the same `./dev.sh cargo ...` command. |

## Container package reproducibility

The development image currently uses `rust:1.93-slim-trixie`; its apt package
versions are intentionally not pinned. This keeps security updates available,
but a historical image cannot yet be rebuilt bit-for-bit. Track a future
container-only change that chooses and documents one of these approaches:

- pin the base image by digest and record the review procedure for refreshing it;
- use a dated Debian snapshot for apt while defining the security-update cadence;
- record resolved apt versions in a build artifact together with the base digest.

Do not freeze package versions indefinitely merely to make an old build
available. That future change must keep the Rust 1.93 toolchain and verify a
fresh `./dev.sh` build after any base-image, snapshot, or package update.
