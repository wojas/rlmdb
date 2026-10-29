#!/bin/sh
# Run cargo commands for the Rust workspace inside a Linux (Debian, glibc)
# container. Used to test the Linux lock-file layout (process-shared robust
# pthread mutexes) and 4 KiB pages, which differ from macOS.
#
# Usage: rust/scripts/linux-test.sh [cargo args...]
#   default: test --workspace
# Environment:
#   LINUX_TEST_IMAGE  image to use (default rust:1-bookworm)
#
# The workspace is bind-mounted read-write at /work; build output goes to a
# named volume so host and container builds never mix, and the cargo
# registry is cached in another named volume.
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
repo=$(cd "$here/.." && pwd)
image=${LINUX_TEST_IMAGE:-rust:1-bookworm}
# Docker Desktop's credential helper is not installed on this machine, so use
# a scratch config and talk to the Lima docker socket directly.
export DOCKER_CONFIG=${DOCKER_CONFIG:-/tmp/lmdb-rs-dockercfg}
mkdir -p "$DOCKER_CONFIG"
[ -f "$DOCKER_CONFIG/config.json" ] || echo '{}' > "$DOCKER_CONFIG/config.json"
if [ -z "${DOCKER_HOST:-}" ] && [ -S "$HOME/.lima/docker/sock/docker.sock" ]; then
  export DOCKER_HOST="unix://$HOME/.lima/docker/sock/docker.sock"
fi
if [ $# -eq 0 ]; then set -- test --workspace; fi
exec docker run --rm -i \
  -v "$repo:/work" \
  -v lmdb-rs-target-linux:/work/rust/target \
  -v lmdb-rs-cargo-registry:/usr/local/cargo/registry \
  -w /work/rust \
  -e CARGO_TERM_COLOR=never \
  "$image" cargo "$@"
