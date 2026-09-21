#!/usr/bin/env bash
# Run cargo test inside a Linux Docker container — the environment CI
# actually tests in.
#
# Why this exists: not every code path is exercisable on a macOS host.
# The security-test corpus canaries count open file descriptors through
# /proc/self/fd on Linux (/dev/fd on macOS), and platform-wake-emission
# compiles futex-backed wake emission only on Linux (a no-op on macOS).
# A green local test run therefore does not imply a green Linux CI run;
# this script runs the exact test command inside a Linux container so
# local results match CI.
#
# Usage:
#   scripts/test.sh                                    # cargo test (CI default job)
#   scripts/test.sh --features interpreter             # aot + interpreter
#   scripts/test.sh --features security-test           # security-test job
#   scripts/test.sh --features platform-wake-emission  # Linux-only wake emission
#   scripts/test.sh -p wasmtiny-aotc                   # one workspace member
#
# Environment overrides:
#   RUST_IMAGE       Docker image tag (default: rust:1.98, CI's current stable series)
#   TOOLCHAIN        Pin a rustup toolchain inside the container (e.g. 1.98.1)
#   DOCKER_PLATFORM  Emulate a platform (e.g. linux/amd64; slow under qemu)
#
# Named volumes cache the toolchain, registry and build artefacts across
# runs (wasmtiny-rustup / wasmtiny-cargo / wasmtiny-target). Cargo is
# pointed at the container target volume, so Linux build products never
# land in the host target/ directory and never mix with macOS artefacts.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"

# ---------------------------------------------------------------------------
# Ensure the Docker daemon is up (best effort on macOS via Docker Desktop).
# ---------------------------------------------------------------------------
if ! docker info >/dev/null 2>&1; then
  if [ "$(uname -s)" = "Darwin" ]; then
    echo "test.sh: starting Docker Desktop..."
    open -a Docker
    for _ in $(seq 1 60); do
      docker info >/dev/null 2>&1 && break
      sleep 2
    done
  fi
  if ! docker info >/dev/null 2>&1; then
    echo "test.sh: Docker daemon is not running." >&2
    exit 1
  fi
fi

# ---------------------------------------------------------------------------
# Assemble the run.
# ---------------------------------------------------------------------------
RUST_IMAGE="${RUST_IMAGE:-rust:1.98}"

PLATFORM_ARGS=()
if [ -n "${DOCKER_PLATFORM:-}" ]; then
  PLATFORM_ARGS=(--platform "$DOCKER_PLATFORM")
fi

TTY_ARGS=()
if [ -t 1 ]; then
  TTY_ARGS=(-t)
fi

# No arguments means the exact CI default (`cargo test`); anything else
# is passed through verbatim (feature combinations in CI, one crate, ...).

TOOLCHAIN_SETUP=":"
if [ -n "${TOOLCHAIN:-}" ]; then
  TOOLCHAIN_SETUP="rustup toolchain install '$TOOLCHAIN' >/dev/null; export RUSTUP_TOOLCHAIN='$TOOLCHAIN'"
fi

echo "test.sh: image=$RUST_IMAGE workspace=/work"
echo "test.sh: cargo test $*"

# The first run seeds the named volumes and builds the dependency tree with
# codegen (several minutes); subsequent runs are incremental.
docker run --rm \
  "${PLATFORM_ARGS[@]}" \
  "${TTY_ARGS[@]}" \
  -v "$ROOT":/work \
  -v wasmtiny-rustup:/usr/local/rustup \
  -v wasmtiny-cargo:/usr/local/cargo \
  -v wasmtiny-target:/target \
  -w /work \
  -e CARGO_INCREMENTAL=0 \
  -e CARGO_TARGET_DIR=/target \
  -e CARGO_TERM_COLOR=always \
  "$RUST_IMAGE" \
  bash -c '
    set -euo pipefail
    { '"$TOOLCHAIN_SETUP"'; }
    exec cargo test "$@"
  ' bash "$@"
