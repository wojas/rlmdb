#!/bin/sh
# Run a command for the Rust workspace in a Termux container
# (termux/termux-docker): Android's C library (bionic) and Termux's
# packages (Rust, clang) on the Docker host's Linux kernel. This is how
# rlmdb is tested on Android without a device or an emulator. The tests
# build the C reference libraries with Termux's clang against bionic, as
# the Rust code is built, so C and Rust share Android's lock file there.
#
# Usage: scripts/termux-test.sh [command...]
#   default: cargo test --workspace --no-fail-fast
# Environment:
#   TERMUX_TEST_IMAGE  image to use (default termux/termux-docker with the
#                      host's architecture as its tag: x86_64 or aarch64)
#   TERMUX_TEST_MEMORY memory limit of the container (docker run --memory;
#                      default 2g, see AGENTS.md section 8; empty or 0 for
#                      no limit)
#   TERMUX_TEST_KEEP   1 keeps the container (named rlmdb-termux-<pid>)
#   RLMDB_TEST_SCOPE   passed on to the tests
#
# The checkout is copied into the container (without .git and target), so
# each run builds from scratch. The container is not privileged and mounts
# nothing.
set -eu

cd "$(dirname "$0")/.."

arch=$(docker info --format '{{.Architecture}}')
case "$arch" in
    x86_64 | amd64) tag=x86_64 ;;
    aarch64 | arm64) tag=aarch64 ;;
    *)
        echo "termux-test.sh: no Termux image for $arch" >&2
        exit 1
        ;;
esac
image=${TERMUX_TEST_IMAGE:-termux/termux-docker:$tag}
memory=${TERMUX_TEST_MEMORY-2g}
name=rlmdb-termux-$$

if [ $# -eq 0 ]; then
    set -- cargo test --workspace --no-fail-fast
fi
# The command, quoted for the shell in the container.
cmd=
for a in "$@"; do
    cmd="$cmd '$(printf '%s' "$a" | sed "s/'/'\\\\''/g")'"
done

limit=
if [ -n "$memory" ] && [ "$memory" != 0 ]; then
    limit="--memory $memory --memory-swap $memory"
fi
# shellcheck disable=SC2086 # $limit is two options or nothing
docker run -d --name "$name" $limit "$image" sleep infinity >/dev/null
cleanup() {
    if [ "${TERMUX_TEST_KEEP:-}" != 1 ]; then
        docker rm -f "$name" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT

# Termux's login shell, as its own user, sets PATH, HOME and TMPDIR.
termux() {
    docker exec -i -u system "$name" /entrypoint.sh bash -lc "$1"
}

termux 'pkg update -y >/dev/null && pkg install -y rust clang getconf >/dev/null'
COPYFILE_DISABLE=1 tar -c --exclude ./.git --exclude ./target . |
    termux 'mkdir -p ~/rlmdb && tar -x -C ~/rlmdb'
termux "cd ~/rlmdb && rustc --version && clang --version | head -1 &&
    export RLMDB_TEST_SCOPE='${RLMDB_TEST_SCOPE:-}' && $cmd"
