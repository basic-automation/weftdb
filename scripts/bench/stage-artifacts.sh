#!/usr/bin/env bash
# Stage one bench run's artifacts for upload as a GitHub Actions artifact (EVI-4).
#
# The artifact pipeline is this script plus the contract around it, not a store:
# a bench-producing job stages its run's files flat into one directory and uploads
# that directory as the `bench-report` artifact with a 90-day retention. The shape
# is the one EVI-5's nightly gate established, so a consumer that downloads the
# artifact finds the report by name at the top level: a nested layout would break
# the next run's baseline download, which looks for `<report>.json` at the top.
#
# Contract (any job that publishes bench artifacts follows it):
#   - the artifact name is `bench-report`;
#   - the retention is 90 days;
#   - the directory is flat: the report, its run manifest, and any compare
#     verdict sit at the top level, never nested under a workload directory;
#   - the run artifacts themselves are never committed: Actions storage is the
#     only home for them (the README's "run artifacts are not committed" stands).
# The claims registry (bench/claims.json) records, per figure, which workflow
# publishes its artifact, if any; tools/check_claims.py verifies that the named
# workflow really uploads an artifact of that name.
#
# Usage:
#   stage-artifacts.sh --report <report.json> [--manifest <manifest.json>]
#                      [--out <dir>] [extra-file ...]
# The report is required. The manifest and the extra files (a compare verdict,
# the compare transcript) are optional: a file that does not exist is skipped,
# so the same call stages a first run (report + manifest only) and a compared
# run (report + manifest + verdict) without the caller branching. A missing
# report is an error, because an artifact with no report is not evidence and
# must not silently become the next run's baseline. Exit 2 on usage errors.
set -euo pipefail

report=""
manifest=""
out="bench-stage"

while [ $# -gt 0 ]; do
  case "$1" in
    --report)
      [ $# -ge 2 ] || { echo "stage-artifacts: --report needs a value" >&2; exit 2; }
      report="$2"; shift 2 ;;
    --manifest)
      [ $# -ge 2 ] || { echo "stage-artifacts: --manifest needs a value" >&2; exit 2; }
      manifest="$2"; shift 2 ;;
    --out)
      [ $# -ge 2 ] || { echo "stage-artifacts: --out needs a value" >&2; exit 2; }
      out="$2"; shift 2 ;;
    --)
      shift; break ;;
    -*)
      echo "stage-artifacts: unknown option $1" >&2; exit 2 ;;
    *)
      break ;;
  esac
done

if [ -z "$report" ]; then
  echo "stage-artifacts: --report is required" >&2
  exit 2
fi
if [ ! -f "$report" ]; then
  echo "stage-artifacts: report $report does not exist; refusing to stage an empty artifact" >&2
  exit 2
fi

mkdir -p "$out"
cp "$report" "$out/"
if [ -n "$manifest" ] && [ -f "$manifest" ]; then
  cp "$manifest" "$out/"
fi
for extra in "$@"; do
  if [ -f "$extra" ]; then
    cp "$extra" "$out/"
  fi
done

echo "stage-artifacts: staged into $out:"
ls -1 "$out"
