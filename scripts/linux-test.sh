#!/bin/sh
# Run cargo commands for the Rust workspace inside a Linux (Debian, glibc)
# container. Used to test the Linux lock-file layout (process-shared robust
# pthread mutexes) and 4 KiB pages, which differ from macOS.
#
# Usage: scripts/linux-test.sh [cargo args...]
#   default: test --workspace
# Environment:
#   LINUX_TEST_IMAGE  image to use (default rust:1-bookworm)
#   LINUX_TEST_TMPFS  size of the tmpfs mounted at /tmp in the container
#                     (default 4g; empty or 0 leaves /tmp on the VM disk)
#
# /tmp, where tests and benchmarks create their environments, is a tmpfs in
# the VM's memory: the alternatives inside the VM are its virtual disk
# (overlay on a disk image) or the virtiofs host mount, and neither is a
# local file system. fsync workloads are therefore not meaningful here.
#
# The checkout is bind-mounted read-write at /work; build output goes to a
# named volume so host and container builds never mix, and the cargo
# registry is cached in another named volume. The target volume is per
# checkout (its name carries a checksum of the checkout's path): every
# checkout is mounted at the same /work, so cargo cannot tell their
# artefacts apart and a shared volume served stale builds of another
# worktree. The C reference sources are in the checkout (tests/lmdb-c), so
# the container needs nothing else from the host.
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
image=${LINUX_TEST_IMAGE:-rust:1-bookworm}
tmpfs=${LINUX_TEST_TMPFS-4g}
case $tmpfs in ""|0) tmpfs_mount= ;; *) tmpfs_mount="--tmpfs /tmp:exec,size=$tmpfs" ;; esac
# Docker Desktop's credential helper is not installed on this machine, so use
# a scratch config and talk to the Lima docker socket directly.
export DOCKER_CONFIG=${DOCKER_CONFIG:-/tmp/lmdb-rs-dockercfg}
mkdir -p "$DOCKER_CONFIG"
[ -f "$DOCKER_CONFIG/config.json" ] || echo '{}' > "$DOCKER_CONFIG/config.json"
if [ -z "${DOCKER_HOST:-}" ] && [ -S "$HOME/.lima/docker/sock/docker.sock" ]; then
  export DOCKER_HOST="unix://$HOME/.lima/docker/sock/docker.sock"
fi
if [ $# -eq 0 ]; then set -- test --workspace; fi
target_vol="lmdb-rs-target-linux-$(printf '%s' "$here" | cksum | cut -d' ' -f1)"
# shellcheck disable=SC2086  # $tmpfs_mount is two words or empty
exec docker run --rm -i \
  $tmpfs_mount \
  -v "$here:/work" \
  -v "$target_vol:/work/target" \
  -v lmdb-rs-cargo-registry:/usr/local/cargo/registry \
  -w /work \
  -e CARGO_TERM_COLOR=never \
  "$image" sh -c 'case $1 in clippy) rustup component add clippy >/dev/null 2>&1 ;; esac; exec cargo "$@"' sh "$@"
