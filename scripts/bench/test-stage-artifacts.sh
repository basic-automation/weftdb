#!/usr/bin/env bash
# Tests for scripts/bench/stage-artifacts.sh (EVI-4). Run from the repo root.
set -euo pipefail

SCRIPT="scripts/bench/stage-artifacts.sh"
fail=0

check() { # name, condition-command...
  local name="$1"; shift
  if "$@"; then
    echo "ok - $name"
  else
    echo "FAIL - $name"
    fail=1
  fi
}

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# A fake run: a report, its manifest, a compare verdict and transcript.
mkdir -p "$tmp/run"
printf '{"workload":"nightly-interpolation"}\n' > "$tmp/run/workload__weftdb.json"
printf '{"report":"workload__weftdb.json"}\n' > "$tmp/run/workload__weftdb.manifest.json"
printf '{"verdict":"flat"}\n' > "$tmp/run/verdict.json"
printf 'regression gate\n' > "$tmp/run/compare.txt"

# Happy path: everything lands flat, no nesting.
"$SCRIPT" --report "$tmp/run/workload__weftdb.json" \
  --manifest "$tmp/run/workload__weftdb.manifest.json" \
  --out "$tmp/stage" "$tmp/run/verdict.json" "$tmp/run/compare.txt" > "$tmp/log"
check "stages report flat" test -f "$tmp/stage/workload__weftdb.json"
check "stages manifest flat" test -f "$tmp/stage/workload__weftdb.manifest.json"
check "stages verdict flat" test -f "$tmp/stage/verdict.json"
check "stages transcript flat" test -f "$tmp/stage/compare.txt"
check "no nested directories" test "$(find "$tmp/stage" -mindepth 1 -type d | wc -l)" -eq 0

# First run: no verdict files exist yet, but report + manifest still stage.
"$SCRIPT" --report "$tmp/run/workload__weftdb.json" \
  --manifest "$tmp/run/workload__weftdb.manifest.json" \
  --out "$tmp/stage-first" "$tmp/run/verdict-missing.json" > "$tmp/log-first"
check "first run stages report" test -f "$tmp/stage-first/workload__weftdb.json"
check "first run stages manifest" test -f "$tmp/stage-first/workload__weftdb.manifest.json"
check "missing optional file is skipped, not an error" test "$(ls "$tmp/stage-first" | wc -l)" -eq 2

# Missing report: an empty artifact must never be staged.
if "$SCRIPT" --report "$tmp/run/no-such.json" --out "$tmp/stage-empty" > "$tmp/log-empty" 2>&1; then
  echo "FAIL - missing report exits non-zero"
  fail=1
else
  echo "ok - missing report exits non-zero"
fi
check "missing report stages nothing" test ! -e "$tmp/stage-empty/workload__weftdb.json"

# Missing --report entirely, and an unknown option, are usage errors.
if "$SCRIPT" --out "$tmp/stage-nousage" > /dev/null 2>&1; then
  echo "FAIL - missing --report exits non-zero"; fail=1
else
  echo "ok - missing --report exits non-zero"
fi
if "$SCRIPT" --bogus > /dev/null 2>&1; then
  echo "FAIL - unknown option exits non-zero"; fail=1
else
  echo "ok - unknown option exits non-zero"
fi

exit "$fail"
