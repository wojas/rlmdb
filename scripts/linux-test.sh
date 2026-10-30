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
# registry is cached in another named volume. The target volume is per
# checkout (its name carries a checksum of the checkout's path): every
# checkout is mounted at the same /work, so cargo cannot tell their
# artefacts apart and a shared volume served stale builds of another
# worktree.
#
# interop/build.rs extracts the LMDB 1.0 sources with `git show`, so the
# container needs the repository's git directory too. In a linked worktree
# `.git` is a file naming the main repository's `.git/worktrees/<name>` by
# absolute path, outside the checkout: that common git directory is mounted
# read-only at the same path. Git inside runs as root on files it may not
# own, so its ownership check is switched off (container only).
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
target_vol="lmdb-rs-target-linux-$(printf '%s' "$repo" | cksum | cut -d' ' -f1)"
# The common git directory, if it is outside the checkout (linked worktree).
git_mount=
if git_common=$(cd "$repo" && git rev-parse --git-common-dir 2>/dev/null); then
  case $git_common in
    /*) ;;
    *) git_common=$repo/$git_common ;;
  esac
  git_common=$(cd "$git_common" && pwd)
  case $git_common/ in
    "$repo"/*) ;;
    *) git_mount=$git_common ;;
  esac
fi
exec docker run --rm -i \
  -v "$repo:/work" \
  ${git_mount:+-v "$git_mount:$git_mount:ro"} \
  -e GIT_CONFIG_COUNT=1 -e GIT_CONFIG_KEY_0=safe.directory -e GIT_CONFIG_VALUE_0='*' \
  -v "$target_vol:/work/rust/target" \
  -v lmdb-rs-cargo-registry:/usr/local/cargo/registry \
  -w /work/rust \
  -e CARGO_TERM_COLOR=never \
  "$image" cargo "$@"
