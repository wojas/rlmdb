#!/bin/sh
# Run every check of the Rust workspace: formatting, clippy, rustdoc and the
# tests on the host, then the tests in the Linux container
# (scripts/linux-test.sh). Every step runs even if an earlier one failed;
# a PASS/FAIL summary per step is printed at the end, and the exit status
# is 1 if any step failed.
#
# Usage: rust/scripts/check-all.sh [--no-linux]
#   --no-linux  skip the Linux container step (reported as SKIP)
# Works from any directory.
set -u
here=$(cd "$(dirname "$0")/.." && pwd)
cd "$here" || exit 1

linux=1
for arg in "$@"; do
  case $arg in
    --no-linux) linux=0 ;;
    -h|--help) sed -n '2,10p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "check-all.sh: unknown argument: $arg" >&2; exit 2 ;;
  esac
done

summary=""
failed=0

# step <name> <command...>: runs the command, records PASS/FAIL and time.
step() {
  name=$1
  shift
  echo
  echo "==> $name: $*"
  start=$(date +%s)
  if "$@"; then
    result=PASS
  else
    result=FAIL
    failed=1
  fi
  secs=$(($(date +%s) - start))
  summary="$summary$(printf '%-6s %-40s %5ss' "$result" "$name" "$secs")
"
}

step "fmt --check" cargo fmt --all -- --check
step "clippy -D warnings" cargo clippy --workspace --all-targets -- -D warnings
step "doc -D warnings" env RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
step "test (host: $(uname -s))" cargo test --workspace --no-fail-fast
if [ "$linux" = 1 ]; then
  step "test (Linux container)" "$here/scripts/linux-test.sh" test --workspace --no-fail-fast
else
  summary="$summary$(printf '%-6s %-40s' SKIP 'test (Linux container)')
"
fi

echo
echo "==> Summary"
printf '%s' "$summary"
if [ "$failed" = 0 ]; then
  echo "ALL PASSED"
else
  echo "SOME CHECKS FAILED"
fi
exit "$failed"
