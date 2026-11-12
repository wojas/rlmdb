#!/bin/sh
# Build the documentation site into site/:
#
#   site/       the pages of docs/, built with Zensical in strict mode
#               (a broken link between pages fails the build)
#   site/api/   the API documentation of the rlmdb crate, from rustdoc
#
# docs/AGENTS.md says how the pages are written. The site is published
# at https://rlmdb.wojas.nl/ by .github/workflows/docs.yml, which runs
# this script.
#
# Usage: scripts/build-docs.sh [--no-api]
#   --no-api   build the pages only, without the Rust API documentation
#              (quicker while writing; the links into api/ then lead
#              nowhere)
#
# Zensical is taken from PATH (pip install -r docs/requirements.txt), or
# run through uvx with the release pinned in docs/requirements.txt.
# Works from any directory.
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
cd "$here"

api=1
while [ $# -gt 0 ]; do
  case $1 in
    --no-api) api=0; shift ;;
    -h|--help) sed -n '2,19p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "build-docs.sh: unknown argument: $1" >&2; exit 2 ;;
  esac
done

if command -v zensical >/dev/null 2>&1; then
  zensical build --clean --strict
elif command -v uvx >/dev/null 2>&1; then
  uvx --with-requirements docs/requirements.txt zensical build --clean --strict
else
  echo "build-docs.sh: zensical not found (pip install -r docs/requirements.txt)" >&2
  exit 1
fi

[ "$api" = 1 ] || exit 0

# A target directory of its own, so that site/api/ holds the rlmdb crate
# and nothing an earlier `cargo doc` of other crates left behind.
"${CARGO:-cargo}" doc -p rlmdb --no-deps --locked --target-dir target/site-api
rm -rf site/api
mkdir -p site/api
cp -R target/site-api/doc/. site/api/
# rustdoc writes no page for the directory itself.
cat > site/api/index.html <<'EOF'
<!doctype html>
<meta charset="utf-8">
<title>rlmdb API</title>
<meta http-equiv="refresh" content="0; url=rlmdb/index.html">
<a href="rlmdb/index.html">rlmdb API documentation</a>
EOF
echo "built site/ with the Rust API documentation in site/api/"
