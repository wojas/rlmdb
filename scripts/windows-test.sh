#!/bin/sh
# Run a command in a copy of this checkout on a Windows machine, over ssh to
# its OpenSSH server (cmd.exe as the remote shell).
#
# Usage: scripts/windows-test.sh [command...]
#   default: cargo test --workspace --no-fail-fast
# Environment:
#   WINDOWS_HOST     ssh destination (required), e.g. an alias from ~/.ssh/config
#   WINDOWS_PORT     ssh port (default: what the ssh config says)
#   WINDOWS_WORKDIR  work directory on the Windows side, relative to the
#                    remote home or absolute (default rlmdb-work)
#   WINDOWS_ENV      script in the work directory that sets up the toolchain
#                    for the command (default env.cmd), see below
#   WINDOWS_FULL=1   send every file instead of the changes since last time
#
# The checkout is sent as it is on disk (tracked files and new files that
# are not ignored, through a temporary index; the real index is not
# touched) with `git archive`, as a zip, and unpacked with the `tar` that
# Windows 10 ships, into <workdir>\src. Only files that changed since the
# previous run (whose tree is remembered in this checkout's git directory)
# are sent, so cargo on the Windows side rebuilds only what changed; files
# deleted here are deleted there. No git is needed on Windows, and no
# credentials go there.
#
# <workdir>\<WINDOWS_ENV> is run (`call`) before the command, from <workdir>;
# it sets up the machine's toolchain. `scripts/windows-env.cmd` is that file
# for a work directory that holds its own rustup, cargo and MSYS2 (its
# comments say how to set them up): copy it there as env.cmd.
#
# The command then runs in <workdir>\src, under cmd.exe.
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
host=${WINDOWS_HOST:?set WINDOWS_HOST to the ssh destination of the Windows machine}
workdir=${WINDOWS_WORKDIR:-rlmdb-work}
envfile=${WINDOWS_ENV:-env.cmd}
ssh_opts=
[ -n "${WINDOWS_PORT:-}" ] && ssh_opts="-o Port=$WINDOWS_PORT"
if [ $# -eq 0 ]; then
	set -- cargo test --workspace --no-fail-fast
fi

cd "$here"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/windows-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT

# The tree of the checkout as it is on disk.
cp "$(git rev-parse --git-path index)" "$tmp/index" 2>/dev/null || :
GIT_INDEX_FILE="$tmp/index" git add -A .
tree=$(GIT_INDEX_FILE="$tmp/index" git write-tree)

# What the other side has: the tree sent last time to this host and work
# directory, if any.
state=$(git rev-parse --git-path "windows-test-$(printf '%s' "$host:$workdir" | git hash-object --stdin)")
prev=
if [ -z "${WINDOWS_FULL:-}" ] && [ -f "$state" ]; then
	prev=$(cat "$state")
	git cat-file -e "$prev^{tree}" 2>/dev/null || prev=
fi

if [ -n "$prev" ]; then
	git diff --name-only --no-renames --diff-filter=d "$prev" "$tree" >"$tmp/changed"
	git diff --name-only --no-renames --diff-filter=D "$prev" "$tree" >"$tmp/deleted"
else
	git ls-tree -r --name-only "$tree" >"$tmp/changed"
	: >"$tmp/deleted"
fi
echo "windows-test: $(wc -l <"$tmp/changed" | tr -d ' ') changed, $(wc -l <"$tmp/deleted" | tr -d ' ') deleted file(s)"

# The remote script: delete, unpack, set up, run.
{
	printf '@echo off\r\n'
	printf 'cd /d "%%~dp0"\r\n'
	if [ -z "$prev" ]; then
		printf 'if exist src rmdir /s /q src\r\n'
	fi
	printf 'if not exist src mkdir src\r\n'
	while IFS= read -r f; do
		printf 'if exist "src\\%s" del /f /q "src\\%s"\r\n' "$(echo "$f" | tr / '\\')" "$(echo "$f" | tr / '\\')"
	done <"$tmp/deleted"
	if [ -s "$tmp/changed" ]; then
		printf 'tar -xf windows-test.zip -C src || exit /b 125\r\n'
	fi
	printf 'call "%s" || exit /b 125\r\n' "$envfile"
	printf 'cd src\r\n'
	printf '%s\r\n' "$*"
} >"$tmp/windows-test.cmd"

files="$tmp/windows-test.cmd"
if [ -s "$tmp/changed" ]; then
	# shellcheck disable=SC2046 # one path per line, no spaces in this repository
	git archive --format=zip -o "$tmp/windows-test.zip" "$tree" $(cat "$tmp/changed")
	files="$files $tmp/windows-test.zip"
fi
# shellcheck disable=SC2086
ssh $ssh_opts "$host" "if not exist \"$workdir\" mkdir \"$workdir\""
# shellcheck disable=SC2086
scp -q $ssh_opts $files "$host:$workdir/"
# The other side has this tree once the script got past unpacking (exit
# code 125 means it did not).
rm -f "$state"
rc=0
# shellcheck disable=SC2086
ssh $ssh_opts "$host" "\"$workdir\\windows-test.cmd\"" || rc=$?
[ "$rc" -eq 125 ] || printf '%s\n' "$tree" >"$state"
exit "$rc"
