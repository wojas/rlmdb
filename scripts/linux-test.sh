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
#   LINUX_TEST_TARGET cross-compile for this Rust target triple and run the
#                     tests under QEMU user emulation, for example
#                     s390x-unknown-linux-gnu (big-endian, 64-bit):
#                       LINUX_TEST_TARGET=s390x-unknown-linux-gnu \
#                         scripts/linux-test.sh test -p rlmdb
#
# Cross runs (LINUX_TEST_TARGET): the toolchain runs natively; the image
# (built once, named lmdb-rs-cross-<target>:<recipe checksum>) adds the
# target's GCC cross compilers (gcc- and g++-<arch>-linux-gnu, which also
# build the C oracle), the Rust target and QEMU (qemu-user-static) to
# LINUX_TEST_IMAGE, and the build output has a target volume of its own (the
# checkout's name with -<target> appended). The test binaries and every
# program they start (the C peers, tools and helpers, built for the target
# too) run through the kernel's binfmt_misc handler for the target. The
# container runs privileged so that it can register that handler itself if
# the Docker host has none (a rootless daemon on Linux 6.7 or later has a
# binfmt_misc instance per user namespace, so a handler registered by
# another container is not seen); in CI docker/setup-qemu-action registers
# it on the host. QEMU_LD_PREFIX points the emulator at the cross compiler's
# target libraries. QEMU user mode has no robust futexes (a mutex whose
# owner died is never marked EOWNERDEAD, in C too), which
# INTEROP_NO_ROBUST_MUTEX tells the interop tests. Emulation is slow: run
# one crate at a time; INTEROP_TIMEOUT_SECS, the interop tests' limit per
# program, defaults to 900 s here.
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
cross_env=
cmd=cargo
if [ -n "${LINUX_TEST_TARGET:-}" ]; then
  triple=$LINUX_TEST_TARGET
  gnu=$(printf '%s' "$triple" | sed 's/-unknown-/-/')  # s390x-linux-gnu
  tvar=$(printf '%s' "$triple" | tr 'a-z-' 'A-Z_')     # S390X_UNKNOWN_LINUX_GNU
  cvar=$(printf '%s' "$triple" | tr '-' '_')           # s390x_unknown_linux_gnu
  dockerfile=$(printf '%s\n' "FROM $image" \
    "RUN apt-get update && apt-get install -y --no-install-recommends gcc-$gnu g++-$gnu qemu-user-static && rm -rf /var/lib/apt/lists/*" \
    "RUN rustup target add $triple")
  # The tag follows the recipe, so a changed recipe builds a new image.
  cross_image="lmdb-rs-cross-$triple:$(printf '%s' "$dockerfile" | cksum | cut -d' ' -f1)"
  if ! docker image inspect "$cross_image" >/dev/null 2>&1; then
    printf '%s\n' "$dockerfile" | docker build -q -t "$cross_image" - >&2
  fi
  image=$cross_image
  target_vol="$target_vol-$triple"
  # Every word is one -e NAME=value; values have no spaces.
  cross_env="-e CARGO_BUILD_TARGET=$triple
    -e CARGO_TARGET_${tvar}_LINKER=$gnu-gcc
    -e CC_$cvar=$gnu-gcc -e CXX_$cvar=$gnu-g++ -e AR_$cvar=$gnu-ar
    -e CC=$gnu-gcc -e CXX=$gnu-g++ -e AR=$gnu-ar
    -e QEMU_LD_PREFIX=/usr/$gnu -e INTEROP_NO_ROBUST_MUTEX=1
    -e INTEROP_TIMEOUT_SECS=${INTEROP_TIMEOUT_SECS:-900} --privileged"
  qemu=${gnu%%-*}
  bf=/proc/sys/fs/binfmt_misc
  set -- sh -c "{ [ -e $bf/register ] || mount -t binfmt_misc binfmt_misc $bf; } &&
    { [ -e $bf/qemu-$qemu ] || cat /usr/lib/binfmt.d/qemu-$qemu.conf > $bf/register; } &&
    exec cargo \"\$@\"" sh "$@"
  cmd=
fi
if [ "$cmd" = cargo ]; then
  # Native run: the image has no clippy component; install it on demand.
  set -- sh -c 'case $1 in clippy) rustup component add clippy >/dev/null 2>&1 ;; esac; exec cargo "$@"' sh "$@"
  cmd=
fi
# shellcheck disable=SC2086  # $tmpfs_mount, $cross_env and $cmd are words or empty
exec docker run --rm -i \
  $tmpfs_mount \
  $cross_env \
  -v "$here:/work" \
  -v "$target_vol:/work/target" \
  -v lmdb-rs-cargo-registry:/usr/local/cargo/registry \
  -w /work \
  -e CARGO_TERM_COLOR=never \
  "$image" $cmd "$@"
