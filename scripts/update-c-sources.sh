#!/bin/sh
# Re-create the vendored C reference sources in tests/lmdb-c/0.9 and
# tests/lmdb-c/1.0 (see tests/lmdb-c/README.md): fetch the pinned upstream
# commits into a temporary repository, extract the listed files of
# libraries/liblmdb, apply the patches of tests/lmdb-c/patches/<version>/
# in name order, and report whether the result differs from the tree.
# rlmdb-capi/include/lmdb.h is the 1.0 lmdb.h with a comment in front.
#
# Usage: scripts/update-c-sources.sh [--check]
#   --check  only compare, do not write; exit status 1 if the tree differs
# Works from any directory. Needs git, network access, diff.
set -eu

# Upstream repository and the pinned commits. To move to another commit,
# change it here and run the script.
LMDB_URL=https://github.com/openldap/openldap
# mdb.RE/0.9: the LMDB_0.9.37 release (0.9.36 plus ITS#10615, ITS#10642 and
# ITS#10643, then the release itself: version and man pages).
LMDB09_COMMIT=ff3f48f7ba57ed482567ed23b0019c5eabada293
# mdb.RE/1.0: the LMDB_1.0.3 release (1.0.2 plus the same fixes, then the
# release itself; see interop/README.md).
LMDB10_COMMIT=d4dc8e21ddbcb0c3d6a53b3c19c87559fad2b402

# Files of libraries/liblmdb taken at each commit. The 1.0 list is what
# interop/build.rs builds (LMDB10_LIB_FILES and UPSTREAM10_PROGRAMS).
LMDB09_FILES="mdb.c midl.c midl.h lmdb.h
  mdb_stat.c mdb_dump.c mdb_load.c mdb_copy.c
  mtest.c mtest2.c mtest3.c mtest4.c mtest5.c mtest6.c
  LICENSE COPYRIGHT"
LMDB10_FILES="mdb.c midl.c midl.h lmdb.h module.c chacha8.c chacha8.h
  mdb_stat.c mdb_dump.c mdb_load.c mdb_copy.c mdb_drop.c
  mtest.c mtest2.c mtest3.c mtest4.c mtest5.c
  mtest_remap.c mtest_enc.c mtest_enc3.c
  LICENSE COPYRIGHT"

check=0
for arg in "$@"; do
  case $arg in
    --check) check=1 ;;
    -h|--help) sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "update-c-sources.sh: unknown argument: $arg" >&2; exit 2 ;;
  esac
done

top=$(cd "$(dirname "$0")/.." && pwd)
dest=$top/tests/lmdb-c
tmp=$(mktemp -d "${TMPDIR:-/tmp}/rlmdb-c-sources.XXXXXX")
trap 'rm -rf "$tmp"' EXIT INT TERM
# Keep git from finding a repository above the scratch directory, so that
# `git apply` below works on plain files.
export GIT_CEILING_DIRECTORIES=$tmp

echo "fetching $LMDB09_COMMIT and $LMDB10_COMMIT from $LMDB_URL"
git init -q "$tmp/repo"
git -C "$tmp/repo" fetch -q --depth 1 "$LMDB_URL" "$LMDB09_COMMIT" "$LMDB10_COMMIT"

# extract <version> <commit> <files>: fills $tmp/out/<version>.
extract() {
  out=$tmp/out/$1
  mkdir -p "$out"
  for f in $3; do
    git -C "$tmp/repo" show "$2:libraries/liblmdb/$f" > "$out/$f"
  done
  for p in "$dest/patches/$1"/*.patch; do
    [ -e "$p" ] || continue
    echo "applying patches/$1/$(basename "$p")"
    (cd "$out" && git apply -p1 "$p")
  done
}
extract 0.9 "$LMDB09_COMMIT" "$LMDB09_FILES"
extract 1.0 "$LMDB10_COMMIT" "$LMDB10_FILES"

differs=0
for v in 0.9 1.0; do
  if [ -d "$dest/$v" ] && diff -r "$dest/$v" "$tmp/out/$v" > "$tmp/diff-$v"; then
    echo "tests/lmdb-c/$v: identical to upstream plus patches"
  else
    differs=1
    echo "tests/lmdb-c/$v: DIFFERS from upstream plus patches:"
    if [ -d "$dest/$v" ]; then sed 's/^/  /' "$tmp/diff-$v" | head -40; fi
    if [ "$check" = 0 ]; then
      rm -rf "${dest:?}/$v"
      cp -R "$tmp/out/$v" "$dest/$v"
      echo "tests/lmdb-c/$v: re-created; review with git diff -- tests/lmdb-c/$v"
    fi
  fi
done

# rlmdb-capi/include/lmdb.h, the public header of the rlmdb libraries: the
# 1.0 lmdb.h with this comment in front, generated here so that the two
# cannot drift (rlmdb-capi's tests check it too).
capi_h=$top/rlmdb-capi/include/lmdb.h
{
  cat <<'EOF'
/* lmdb.h of the rlmdb libraries (rlmdb-capi/include).
 *
 * Everything below this comment is the unmodified lmdb.h of LMDB 1.0
 * (tests/lmdb-c/1.0/lmdb.h, the pinned mdb.RE/1.0 release), copied by
 * scripts/update-c-sources.sh. librlmdb and the liblmdb stubs implement
 * this API; rlmdb.h declares rlmdb's own functions, and
 * rlmdb-capi/README.md lists the differences from C LMDB. A program
 * written for LMDB 0.9 may compile against 0.9's lmdb.h instead: the
 * libraries export its functions, with the same ABI.
 */
EOF
  cat "$tmp/out/1.0/lmdb.h"
} > "$tmp/capi-lmdb.h"
if cmp -s "$capi_h" "$tmp/capi-lmdb.h"; then
  echo "rlmdb-capi/include/lmdb.h: the 1.0 lmdb.h with its comment"
else
  differs=1
  echo "rlmdb-capi/include/lmdb.h: DIFFERS from the 1.0 lmdb.h with its comment"
  if [ "$check" = 0 ]; then
    cp "$tmp/capi-lmdb.h" "$capi_h"
    echo "rlmdb-capi/include/lmdb.h: re-created"
  fi
fi
if [ "$check" = 1 ]; then exit "$differs"; fi
