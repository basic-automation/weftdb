#!/usr/bin/env bash
# The crash matrix (D-S21, SOA-9 `crash_kill9`) as one scripted run.
#
# Two matrices, one verdict:
#   1. The subprocess SIGKILL soak: `weft-bench --crash-matrix` runs every
#      failure test (kill-ingest, kill-compaction, corrupt-segment) at every
#      matrix seed, and fails if any cell's reopened store misbehaves.
#   2. The fault-injection crash matrix: `weftdb`'s `swap_tests` crash matrix
#      (errors, power cuts and aborts at every swap point), the orchestration
#      legacy-queue crash test, and the server's poisoned-ready test — the
#      same three suites ci.yml's crash job runs, so this script and that job
#      cannot drift apart silently.
#
# What this script is not: the power-loss matrix. A kill soak cannot see a
# missing fsync, so the privileged power-loss validation (dm-log-writes, or a
# QEMU hard reset on a cache=none disk) stays the owner task the 1.0 plan
# names (decision 20), and this script claims nothing about it.
#
# Artifacts follow EVI-4's pipeline, not an inline loop: with --stage, the
# matrix's reports (one per cell) and their manifests are staged flat through
# scripts/bench/stage-artifacts.sh into one directory, which the caller
# uploads as the `bench-report` artifact (90-day retention). Run artifacts
# are never committed; the staged directory is the artifact, and the first
# successful crash-matrix workflow run becomes the baseline the next run is
# compared against by hand (there is no nightly compare gate for a pass/fail
# matrix — a cell either holds or it does not).
#
# Usage:
#   scripts/crash-matrix.sh [--uci FILE] [--rows N] [--reps N] [--seeds N]
#                           [--seed N] [--stage DIR] [--skip-fault-injection]
# Defaults are the CI matrix: a generated UCI fixture (the same five-row
# household-power shape the bench's own failure tests use, tiled to --rows),
# 3 seeds, 2 reps. --uci names a real UCI household-power file instead.
# Exit non-zero if any cell fails, or if a suite fails to run.
set -euo pipefail

cd "$(dirname "$0")/.."

uci=""
rows=100000
reps=2
seeds=3
seed=0x5EED
stage=""
skip_fault=0

while [ $# -gt 0 ]; do
	case "$1" in
		--uci) [ $# -ge 2 ] || { echo "crash-matrix: --uci needs a file" >&2; exit 2; }; uci="$2"; shift 2 ;;
		--rows) [ $# -ge 2 ] || { echo "crash-matrix: --rows needs a count" >&2; exit 2; }; rows="$2"; shift 2 ;;
		--reps) [ $# -ge 2 ] || { echo "crash-matrix: --reps needs a count" >&2; exit 2; }; reps="$2"; shift 2 ;;
		--seeds) [ $# -ge 2 ] || { echo "crash-matrix: --seeds needs a count" >&2; exit 2; }; seeds="$2"; shift 2 ;;
		--seed) [ $# -ge 2 ] || { echo "crash-matrix: --seed needs a value" >&2; exit 2; }; seed="$2"; shift 2 ;;
		--stage) [ $# -ge 2 ] || { echo "crash-matrix: --stage needs a directory" >&2; exit 2; }; stage="$2"; shift 2 ;;
		--skip-fault-injection) skip_fault=1; shift ;;
		*) echo "crash-matrix: unknown option $1" >&2; exit 2 ;;
	esac
done

work="$(mktemp -d "${TMPDIR:-/tmp}/weft-crash-matrix.XXXXXX")"
trap 'rm -rf "$work"' EXIT

if [ -z "$uci" ]; then
	uci="$work/uci.txt"
	{
		printf '%s\n' "Date;Time;Global_active_power;Global_reactive_power;Voltage;Global_intensity;Sub_metering_1;Sub_metering_2;Sub_metering_3"
		printf '%s\n' "16/12/2006;17:24:00;4.216;0.418;234.840;18.400;0.000;1.000;17.000"
		printf '%s\n' "16/12/2006;17:25:00;5.360;0.436;233.630;23.000;0.000;1.000;16.000"
		printf '%s\n' "16/12/2006;17:26:00;5.374;0.498;233.290;23.000;0.000;2.000;17.000"
		printf '%s\n' "16/12/2006;17:27:00;5.388;0.502;233.740;23.000;0.000;1.000;17.000"
		printf '%s\n' "16/12/2006;17:28:00;3.666;0.528;235.680;15.800;0.000;1.000;17.000"
	} > "$uci"
fi

export PATH="$HOME/.cargo/bin:$PATH"
# Locally the default toolchain is a pinned nightly, so the gate builds and
# tests on stable explicitly. A caller that already selected a toolchain
# (the workflow sets RUSTUP_TOOLCHAIN to the release toolchain) just uses
# plain cargo, which honours that selection.
if [ -n "${RUSTUP_TOOLCHAIN:-}" ]; then
	cargo_cmd="cargo"
else
	cargo_cmd="rustup run stable cargo"
fi
# shellcheck disable=SC2086
# The matrix builds with the release profile, which the workspace tunes for the
# smallest, fastest shipped artifact: whole-program LTO, one codegen unit, line
# tables on. One crate in this workspace — weft-orchestration — does not survive
# that shape on the hosted runner: the job takes SIGTERM about two minutes after
# the crate starts compiling, on every branch, control run 42199040596 on plain
# main included (issue #604). The matrix exercises crash-injection behaviour,
# which does not depend on codegen, so the memory-hungry knobs are relaxed for
# this build alone — opt-level stays 3, and Cargo.toml keeps shipping the tight
# profile untouched.
export CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
export CARGO_PROFILE_RELEASE_LTO=false
export CARGO_PROFILE_RELEASE_DEBUG=false
$cargo_cmd build --locked --release -p weft-bench

out="$work/out"
mkdir -p "$out"
./target/release/weft-bench --ingest --uci "$uci" \
	--crash-matrix --crash-matrix-seeds "$seeds" \
	--failure-rows "$rows" --failure-reps "$reps" --seed "$seed" \
	--out-dir "$out"

# The matrix run writes one report per cell, each beside its manifest (the
# report's stem with `.manifest.json`), because a run manifest grades one
# result. The first report is staged as the report; the rest, and every
# manifest, go along as extra files.
mapfile -t reports < <(find "$out" -maxdepth 1 -name '*.json' ! -name '*.manifest.json' | sort)
[ "${#reports[@]}" -gt 0 ] || { echo "crash-matrix: no report written to $out" >&2; exit 1; }
report="${reports[0]}"
manifest="${report%.json}.manifest.json"
[ -f "$manifest" ] || manifest=""
extras=()
for other in "${reports[@]:1}"; do
	extras+=("$other" "${other%.json}.manifest.json")
done

if [ "$skip_fault" -eq 0 ]; then
	# shellcheck disable=SC2086
	$cargo_cmd test -p weftdb --features fault-injection --locked --test db_tests -- database_new_crash --test-threads=4
	# shellcheck disable=SC2086
	$cargo_cmd test -p weft-orchestration --features fault-injection --locked --test legacy_queue_crash -- --test-threads=4
	# shellcheck disable=SC2086
	$cargo_cmd test -p weft-server --features fault-injection --locked --test ready_poisoned -- --test-threads=4
fi

if [ -n "$stage" ]; then
	if [ -n "$manifest" ]; then
		bash scripts/bench/stage-artifacts.sh --report "$report" --manifest "$manifest" --out "$stage" -- "${extras[@]}"
	else
		bash scripts/bench/stage-artifacts.sh --report "$report" --out "$stage" -- "${extras[@]}"
	fi
	echo "crash-matrix: staged ${#reports[@]} cell reports into $stage"
else
	echo "crash-matrix: ${#reports[@]} cell reports in $out"
fi
