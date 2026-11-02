#!/usr/bin/env bash
#
# smoke-container.sh — prove the WeftDB server container image actually serves (1.0 plan PAC-9).
#
# The image is built by scripts/packaging/build-image.sh; this script is the other half of PAC-9
# and the reason the slice is not just a Dockerfile. It runs the image the way an operator runs
# it — a mounted keys file, a named volume, a published port, no privileges — and checks the
# claims the packaging makes:
#
#   1. `keygen` works inside the image and writes a keys file an operator can mount (SEC-6);
#   2. with no keys file on a non-loopback bind the server refuses to start (SEC-8/C8) rather
#      than serving unauthenticated on 0.0.0.0;
#   3. the container becomes healthy, which exercises the image's own HEALTHCHECK, not a
#      hand-rolled one;
#   4. the exposure matrix holds through the published port: the probes answer anonymously, a
#      read without a key is 401, a wrong-role key is 403, a right-role key is 2xx (SEC-6/SEC-8);
#   5. a declared aspect, an ingested point and a range read round-trip through the store on the
#      volume, as uid 10001 — the container-specific part of the write path (file ownership,
#      locking and fsync on a mount the image did not create);
#   6. a graceful stop and start comes back with the same data, so the volume is doing its job;
#   7. the binary in the finished image still needs only glibc 2.28 (PAC-2's promise, read back
#      out of the artifact with scripts/check-glibc.sh), and the licence files Apache-2.0
#      section 4 requires are in the image.
#
# Usage:
#   scripts/packaging/smoke-container.sh [--image NAME[:TAG]] [--docker COMMAND] [--keep] [--verbose]
#
# --image NAME[:TAG]  the image to test. Default: the tag scripts/packaging/build-image.sh would
#                     have produced for this checkout (weftdb:git-<12 hex>), so the pair runs with
#                     no arguments in CI.
# --docker COMMAND    the container CLI to drive (default: docker).
# --keep              leave the container, the volume and the scratch directory in place for
#                     debugging, and print their names. Nothing else is left behind.
# --verbose           echo every container and HTTP command before it runs.
#
# Requirements on the host: a reachable container daemon, `curl`, and this repository (the glibc
# assertion runs scripts/check-glibc.sh from the checkout). The image itself needs nothing beyond
# its own base: the smoke test speaks HTTP from the host, not from inside the container.
#
# Exit status: 0 when every assertion passed; 1 on the first failed assertion (the container's
# logs are printed); 2 on bad usage or a missing requirement. On any failure the scratch
# resources are removed unless --keep was given.

set -euo pipefail
export LC_ALL=C # byte-wise regexes, as in scripts/release/stage.sh

SCRIPT_NAME=${0##*/}
DOCKER=docker
IMAGE=""
KEEP=0
VERBOSE=0

fail() {
	echo "$SCRIPT_NAME: FAIL: $*" >&2
	if [[ -n ${CONTAINER:-} && $KEEP == 0 ]]; then
		echo "--- last 40 lines of $CONTAINER ---" >&2
		# shellcheck disable=SC2086
		$DOCKER logs --tail 40 "$CONTAINER" >&2 2>&1 || true
	fi
	exit 1
}

usage() {
	sed -n '3,/^set -euo pipefail/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
}

pass() { printf 'ok: %s\n' "$*"; }

# Run a container/HTTP command, echoing it when asked.
run() {
	if ((VERBOSE)); then printf '+ %q ' "$@"; printf '\n' >&2; fi
	# shellcheck disable=SC2086 # $DOCKER is intentionally word-split ("docker --context foo").
	"$@"
}

# ---------------------------------------------------------------------------
# arguments and requirements
# ---------------------------------------------------------------------------
while (($# > 0)); do
	case $1 in
	--image)
		[[ $# -ge 2 ]] || {
			echo "$SCRIPT_NAME: --image needs a value" >&2
			exit 2
		}
		IMAGE=$2
		shift 2
		;;
	--docker)
		[[ $# -ge 2 ]] || {
			echo "$SCRIPT_NAME: --docker needs a value" >&2
			exit 2
		}
		DOCKER=$2
		shift 2
		;;
	--keep)
		KEEP=1
		shift
		;;
	--verbose)
		VERBOSE=1
		shift
		;;
	-h | --help)
		usage
		exit 0
		;;
	*)
		echo "$SCRIPT_NAME: there is no \`$1\` flag; \`$SCRIPT_NAME --help\` lists what this takes" >&2
		exit 2
		;;
	esac
done

command -v curl >/dev/null || {
	echo "$SCRIPT_NAME: this script speaks HTTP from the host and needs \`curl\`" >&2
	exit 2
}
# shellcheck disable=SC2086
$DOCKER info >/dev/null 2>&1 || {
	echo "$SCRIPT_NAME: \`$DOCKER info\` failed: no container daemon is reachable" >&2
	exit 2
}
ROOT=$(git rev-parse --show-toplevel 2>/dev/null) ||
	fail "run this from inside the weftdb repository; the glibc assertion runs scripts/check-glibc.sh"
CHECK_GLIBC=$ROOT/scripts/check-glibc.sh
[[ -x $CHECK_GLIBC ]] || fail "$CHECK_GLIBC is missing or not executable"

if [[ -z $IMAGE ]]; then
	HEAD=$(git -C "$ROOT" rev-parse HEAD)
	IMAGE=weftdb:git-${HEAD:0:12}
fi
# shellcheck disable=SC2086
$DOCKER image inspect "$IMAGE" >/dev/null 2>&1 ||
	fail "no image \`$IMAGE\` locally; build it first: scripts/packaging/build-image.sh --tag $IMAGE"

SUFFIX=${HEAD:-$(${DOCKER} run --rm --entrypoint true "$IMAGE" 2>/dev/null; echo x)}
SUFFIX=$$
CONTAINER=weft-smoke-$SUFFIX
VOLUME=weft-smoke-$SUFFIX
WORK=$(mktemp -d "${TMPDIR:-/tmp}/weft-smoke-XXXXXX")
KEYS=$WORK/keys
GLIBC_FLOOR=2.28

# shellcheck disable=SC2329 # the EXIT trap below is what invokes this
cleanup() {
	local status=$?
	trap - EXIT
	# shellcheck disable=SC2086
	$DOCKER rm --force "$CONTAINER" >/dev/null 2>&1 || true
	# shellcheck disable=SC2086
	$DOCKER volume rm "$VOLUME" >/dev/null 2>&1 || true
	if ((KEEP)); then
		echo "$SCRIPT_NAME: kept $CONTAINER, volume $VOLUME, scratch $WORK"
	elif ((status == 0)); then
		rm -rf "$WORK"
	else
		echo "$SCRIPT_NAME: scratch kept for inspection at $WORK" >&2
	fi
	return "$status"
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------------
# http CODE METHOD PATH [extra curl args...]  — the status code of one request to the container.
http() {
	local method=$1 path=$2
	shift 2
	# shellcheck disable=SC2086
	run curl --silent --show-error --output "$WORK/body" --write-out '%{http_code}' \
		--max-time 20 --noproxy '*' --request "$method" \
		"http://$HOSTPORT$path" "$@" 2>/dev/null
}

# expect WANT DESC METHOD PATH [extra curl args...]
expect() {
	local want=$1 desc=$2
	shift 2
	local got
	got=$(http "$@") || fail "$desc: curl itself failed"
	[[ $got == "$want" ]] || fail "$desc: wanted HTTP $want, got $got (body: $(head -c 300 "$WORK/body" 2>/dev/null || echo -n))"
	pass "$desc -> $got"
}

# body_contains STR DESC — the last response body holds STR.
body_contains() {
	grep -q -- "$1" "$WORK/body" || fail "$2: the response body does not hold \`$1\`: $(head -c 300 "$WORK/body")"
	pass "$2"
}

# keygen ID ROLE — mint a key into the mounted keys file, print the plaintext key.
keygen() {
	local id=$1 role=$2 out
	# shellcheck disable=SC2086
	out=$($DOCKER run --rm --entrypoint /usr/local/bin/weft-server \
		--volume "$KEYS:/keys" "$IMAGE" keygen --id "$id" --role "$role" --keys-file /keys/api.keys) ||
		fail "\`keygen --id $id --role $role\` failed inside the image: $(echo "$out" | tail -3)"
	# The key is printed once, indented four spaces (weft-server/src/keygen.rs).
	printf '%s\n' "$out" | sed -n 's/^    \([A-Za-z0-9._:-]\{16,\}\)$/\1/p' | head -1
}

bearer() { printf 'Authorization: Bearer %s\n' "$1"; }

# ---------------------------------------------------------------------------
# 1. the image's own configuration: non-root, the port, the health check, the labels
# ---------------------------------------------------------------------------
USER_CFG=$($DOCKER image inspect --format '{{.Config.User}}' "$IMAGE")
[[ $USER_CFG == 10001:10001 ]] || fail "the image runs as user \`$USER_CFG\`; PAC-9 requires the unprivileged 10001:10001"
pass "image runs as 10001:10001"

# The format strings below are expanded by docker, not by this shell, which is what SC2016 means
# to catch; the word-splitting of $DOCKER is deliberate ("docker --context foo").
# shellcheck disable=SC2086,SC2016
$DOCKER image inspect --format '{{range $p, $_ := .Config.ExposedPorts}}{{$p}}
{{end}}' "$IMAGE" | grep -q '^8080/tcp$' || fail "the image does not EXPOSE 8080/tcp"
pass "image exposes 8080/tcp"

HEALTH_CFG=$($DOCKER image inspect --format '{{if .Config.Healthcheck}}{{range .Config.Healthcheck.Test}}{{.}} {{end}}{{end}}' "$IMAGE")
[[ $HEALTH_CFG == *weft-health* ]] || fail "the image declares no health check built on /usr/local/bin/weft-health (got \`$HEALTH_CFG\`)"
pass "image declares its health check"

REVISION=$($DOCKER image inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$IMAGE")
[[ $REVISION =~ ^[0-9a-f]{40}$ ]] || fail "the image's OCI revision label is \`$REVISION\`, not a commit sha; a running container must say what it was built from"
pass "image carries org.opencontainers.image.revision=$REVISION"

for doc in NOTICE THIRD-PARTY-LICENSES.md THIRD-PARTY-NOTICES-weft-physical-type THIRD-PARTY-NOTICES-weft-reduce LICENSE-APACHE LICENSE-MIT; do
	# shellcheck disable=SC2086
	$DOCKER run --rm --entrypoint /bin/sh "$IMAGE" -c "test -s /usr/share/doc/weftdb/$doc" ||
		fail "Apache-2.0 section 4: /usr/share/doc/weftdb/$doc is missing or empty in the image"
done
pass "image carries the notices and licence bundle the release archives carry"

# ---------------------------------------------------------------------------
# 2. the keys file, minted inside the image
# ---------------------------------------------------------------------------
mkdir -p "$KEYS"
# The container writes here as uid 10001, which is not this host user; a scratch directory the
# image's uid can write to is what an operator mounting a keys file has to arrange too (see the
# README's container section). 0777 on a mktemp directory is the smallest thing that works
# without asking for root.
chmod 0777 "$KEYS"

READER_KEY=$(keygen smoke-reader reader)
WRITER_KEY=$(keygen smoke-writer writer)
MAINTAINER_KEY=$(keygen smoke-maintainer maintainer)
[[ -s $KEYS/api.keys ]] || fail "keygen inside the image wrote no keys file to the mounted directory"
[[ -n $READER_KEY && -n $WRITER_KEY && -n $MAINTAINER_KEY ]] ||
	fail "could not read the minted keys back from keygen's output (three keys, one indented line each)"
# Read the file back the way the server will read it: from inside the image, as its own uid.
# keygen writes it mode 0600 and the file belongs to 10001, so the host user cannot open it —
# a host-side grep would fail on permissions, not on content, and would then be a check of the
# host rather than of the image. The same uid reads it back, which is the deployment case.
KEYS_DUMP=$($DOCKER run --rm --entrypoint /bin/cat --volume "$KEYS:/keys:ro" "$IMAGE" /keys/api.keys) ||
	fail "the keys file keygen wrote cannot be read back from inside the image as the image's own uid"
for id in smoke-reader smoke-writer smoke-maintainer; do
	printf '%s\n' "$KEYS_DUMP" | grep -qF "$id" || fail "the keys file does not hold the id $id"
done
# No chmod: the file is 0600 owned by the uid the server runs as, and it holds hashes and
# frozen roles rather than working credentials (SEC-6), which is why it is safe to mount
# read-only into the serve container below.
pass "keygen mints and appends a keys file the image can read back as its own uid"

# ---------------------------------------------------------------------------
# 3. the three ways an operator can get a non-loopback bind wrong: refuse to start
# ---------------------------------------------------------------------------
# `weft-server` takes no subcommand word: no arguments *is* serve (packaging/docker/
# Dockerfile's ENTRYPOINT relies on this). Passing the word `serve` is an argument error that
# exits 1 and prints another subcommand's help, which is close enough to a refusal to pass a
# sloppy gate — so these three assert the exit code the configuration refusal actually has,
# 78 (PAC-5b/C7), not merely "non-zero".
expect_refusal() {
	# $1: the phrase the refusal must contain; the rest: docker run arguments.
	local phrase=$1
	shift
	local out rc=0
	# `|| rc=$?`, not `if …; then`: the status of an `if` block that took no branch is 0, which
	# would read back as "the container served" for every refusal.
	out=$($DOCKER run --rm --entrypoint /usr/local/bin/weft-server "$@" 2>&1) || rc=$?
	[[ $rc == 78 ]] ||
		fail "expected the configuration refusal (exit 78) mentioning \`$phrase\`, got exit $rc: $(printf '%s' "$out" | tail -2)"
	printf '%s' "$out" | grep -qF "$phrase" ||
		fail "refused with exit 78 but without the explanation \`$phrase\`: $(printf '%s' "$out" | tail -2)"
}

expect_refusal "no keys file is mounted" \
	-e WEFT_SERVER_ADDR=0.0.0.0:8080 -e WEFT_SEGMENT_STORE_ROOT=/tmp/store "$IMAGE"
pass "no keys file on 0.0.0.0 -> refuses with exit 78, and says why"

expect_refusal "does not name a mounted file" \
	-e WEFT_SERVER_ADDR=0.0.0.0:8080 -e WEFT_SEGMENT_STORE_ROOT=/tmp/store \
	-e WEFT_KEYS_FILE=/keys/api.keys "$IMAGE"
pass "a WEFT_KEYS_FILE that is not mounted -> refuses, naming the path it looked for"

# Keys mounted on the image's default non-loopback bind, with no acknowledgement of the
# plaintext, is refused too (C8). The image binds `0.0.0.0` because a container has to, so
# this is the first configuration an operator actually hits, and the refusal is the point.
expect_refusal "WEFT_ALLOW_PLAINTEXT is not set" \
	--volume "$KEYS:/run/weft:ro" -e WEFT_SERVER_ADDR=0.0.0.0:8080 \
	-e WEFT_SEGMENT_STORE_ROOT=/tmp/store -e WEFT_KEYS_FILE=/run/weft/api.keys "$IMAGE"
pass "keys on a non-loopback bind, unacknowledged -> refuses, naming WEFT_ALLOW_PLAINTEXT"

# ---------------------------------------------------------------------------
# 4. serve with the keys file mounted, on a volume
# ---------------------------------------------------------------------------
# WEFT_ALLOW_PLAINTEXT is set because the image's bind is `0.0.0.0`, which is what makes a
# published container port reachable at all. A deployment that terminates TLS in front of the
# container, or publishes it to a private network, is exactly what the flag acknowledges; the
# README's container section says the same thing to the operator.
# shellcheck disable=SC2086
$DOCKER volume create "$VOLUME" >/dev/null
start_container() {
	# shellcheck disable=SC2086
	$DOCKER start "$CONTAINER" >/dev/null
	host_port # a restart re-picks the ephemeral host port; the old one is dead
}
# shellcheck disable=SC2086
$DOCKER run --detach --name "$CONTAINER" \
	--publish '127.0.0.1::8080' \
	--volume "$KEYS:/run/weft:ro" \
	--volume "$VOLUME:/var/lib/weftdb" \
	-e WEFT_KEYS_FILE=/run/weft/api.keys -e WEFT_ALLOW_PLAINTEXT=1 \
	-e WEFT_STORE_DATABASE=smoke -e WEFT_STORE_SUBJECT=plant \
	"$IMAGE" >/dev/null || fail "the container did not start"

wait_healthy() {
	local deadline=$((SECONDS + 120)) state
	while ((SECONDS < deadline)); do
		state=$($DOCKER inspect \
			--format '{{.State.Status}} {{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}' \
			"$CONTAINER" 2>/dev/null || echo "gone none")
		[[ ${state%% *} == running ]] || fail "the container is \`$state\`, not running"
		[[ ${state##* } == healthy ]] && return 0
		sleep 2
	done
	fail "the container never reported healthy (120s); its health check output: $($DOCKER inspect --format '{{json .State.Health}}' "$CONTAINER" 2>/dev/null | tail -c 400)"
}
wait_healthy
pass "container is healthy through the image's own HEALTHCHECK"

# `--publish 127.0.0.1::8080` asks for an ephemeral host port, and `docker start` picks a *new*
# one on every restart, so this is re-read after each start rather than captured once.
host_port() {
	HOSTPORT=$($DOCKER port "$CONTAINER" 8080/tcp | head -1)
	[[ $HOSTPORT =~ ^127[.]0[.]0[.]1:[0-9]+$ ]] || fail "no published loopback port for 8080 (got \`$HOSTPORT\`)"
}
host_port
pass "published at $HOSTPORT"

UID_IN=$($DOCKER exec "$CONTAINER" id -u)
[[ $UID_IN == 10001 ]] || fail "the running server is uid $UID_IN, not 10001"
pass "the running server is unprivileged (uid 10001)"

# ---------------------------------------------------------------------------
# 5. the exposure matrix through the published port
# ---------------------------------------------------------------------------
expect 200 "anonymous GET /health" GET /health
expect 401 "GET /api/v1/storage/catalog with no key" GET /api/v1/storage/catalog
expect 401 "GET /api/v1/storage/catalog with a bogus key" GET /api/v1/storage/catalog \
	--header "$(bearer weft_0123456789abcdef0123456789abcdef)"
expect 200 "reader key GET /api/v1/storage/catalog" GET /api/v1/storage/catalog \
	--header "$(bearer "$READER_KEY")"
expect 403 "writer key GET /metrics (no Metrics scope)" GET /metrics --header "$(bearer "$WRITER_KEY")"
expect 200 "maintainer key GET /metrics" GET /metrics --header "$(bearer "$MAINTAINER_KEY")"
# The prefix the registry actually emits is `weft_` (weft-server/src/metrics.rs:422-431), not
# `weftdb_`; a smoke test that greps the wrong prefix fails on a server that is fine.
grep -q '^weft_' "$WORK/body" || fail "GET /metrics answered but with no \`weft_*\` metrics: $(head -c 200 "$WORK/body")"
pass "the metrics endpoint serves WeftDB metrics"

# ---------------------------------------------------------------------------
# 6. a real round trip: declare, ingest, read back — all on the volume
# ---------------------------------------------------------------------------
expect 403 "reader key POST /api/v1/storage/aspects (no Declare scope)" POST /api/v1/storage/aspects \
	--header "$(bearer "$READER_KEY")" --header 'Content-Type: application/json' \
	--data '{"name":"smoke_aspect","physical_type":"f64","timestamp_unit":"millis"}'
expect 201 "maintainer key declares an aspect" POST /api/v1/storage/aspects \
	--header "$(bearer "$MAINTAINER_KEY")" --header 'Content-Type: application/json' \
	--data '{"name":"smoke_aspect","physical_type":"f64","timestamp_unit":"millis"}'
expect 403 "reader key POST /smoke_aspect/points (no Ingest scope)" POST /api/v1/storage/smoke_aspect/points \
	--header "$(bearer "$READER_KEY")" --header 'Content-Type: application/json' \
	--data '{"points":[{"timestamp":1700000000000,"value":"3.5"}]}'
expect 201 "writer key ingests a point" POST /api/v1/storage/smoke_aspect/points \
	--header "$(bearer "$WRITER_KEY")" --header 'Content-Type: application/json' \
	--data '{"batch_id":"smoke-1","points":[{"timestamp":1700000000000,"value":"3.5"}]}'
# `/points` is the JSON read of a time range (`/range` is the Arrow IPC one, and
# `/range.csv` the CSV one: weft-server/src/routes.rs:828-831). The bounds are `start`/`end`, not
# `from`/`to` — a `from=` query is a 400 (`missing field \`start\``), which is exactly the kind of
# contract a smoke test gets to find out by asking.
expect 200 "reader key reads the point back" GET '/api/v1/storage/smoke_aspect/points?start=0&end=2000000000000' \
	--header "$(bearer "$READER_KEY")"
body_contains '3.5' "the range read holds the ingested value"
body_contains '1700000000000' "the range read holds the ingested timestamp"

# The columnar read path works in the container too. The claim asserted is the response's content
# type, not the `ARROW1` magic: this writer emits the continuation marker `FF FF FF FF` straight
# into the stream (verified with `od -c` on a live response) rather than the 8-byte magic, so
# grepping for `ARROW1` fails on a perfectly good Arrow stream.
expect 200 "reader key reads the Arrow range" GET '/api/v1/storage/smoke_aspect/range?start=0&end=2000000000000' \
	--header "$(bearer "$READER_KEY")" --dump-header "$WORK/headers"
grep -qi '^content-type: application/vnd.apache.arrow.stream' "$WORK/headers" ||
	fail "the range read did not come back as an Arrow stream: $(grep -i '^content-type' "$WORK/headers" || echo 'no content-type header')"
[[ -s $WORK/body ]] || fail "the Arrow range read returned an empty body"
pass "the Arrow read path answers from the container ($(wc -c < "$WORK/body") bytes)"

# The counter moved, so the ingest really went through the server's storage path and was not
# merely accepted into a queue that then dropped it.
expect 200 "maintainer key reads /metrics after the ingest" GET /metrics --header "$(bearer "$MAINTAINER_KEY")"
grep -qE '^weft_ingest_rows_sealed_total [1-9]' "$WORK/body" ||
	fail "the ingest was accepted but \`weft_ingest_rows_sealed_total\` is still zero: $(grep -E '^weft_ingest' "$WORK/body" | head -3)"
pass "the metrics count the sealed row"

# The store really is on the volume, written by uid 10001.
# shellcheck disable=SC2016 # the $(ls -A) runs in the container's sh, not here
$DOCKER run --rm --entrypoint /bin/sh --volume "$VOLUME:/var/lib/weftdb" "$IMAGE" -c \
	'test -d /var/lib/weftdb/store && test -n "$(ls -A /var/lib/weftdb/store)"' ||
	fail "the store root on the volume is missing or empty after an ingest"
pass "the ingest landed in the store on the named volume"

# ---------------------------------------------------------------------------
# 7. a graceful stop and a restart keep the data
# ---------------------------------------------------------------------------
# shellcheck disable=SC2086
$DOCKER stop --time 30 "$CONTAINER" >/dev/null || fail "the container did not stop within its grace"
EXITED=$($DOCKER inspect --format '{{.State.ExitCode}}' "$CONTAINER")
[[ $EXITED == 0 ]] || fail "a SIGTERM stop exited $EXITED, not 0 (the drain path should exit clean)"
pass "SIGTERM drains and exits 0"

start_container
wait_healthy
expect 200 "reader key reads the aspect after a restart" GET '/api/v1/storage/smoke_aspect/points?start=0&end=2000000000000' \
	--header "$(bearer "$READER_KEY")"
body_contains '3.5' "the value survived a stop and start"

# ---------------------------------------------------------------------------
# 8. the artifact claims: the glibc floor, read back out of the image
# ---------------------------------------------------------------------------
PROBE=$($DOCKER create --entrypoint /bin/true "$IMAGE")
$DOCKER cp "$PROBE:/usr/local/bin/weft-server" "$WORK/weft-server"
$DOCKER rm --force "$PROBE" >/dev/null
# `check-glibc.sh` takes `[--verbose] <floor> <artifact>...` and has no `--quiet`; passing one makes
# it read the flag as the floor and refuse ("floor --quiet is not a version like 2.28").
$CHECK_GLIBC "$GLIBC_FLOOR" "$WORK/weft-server" ||
	fail "the binary inside the image needs glibc newer than $GLIBC_FLOOR: the image's builder no longer matches the release baseline"
pass "the image's binary holds the glibc $GLIBC_FLOOR floor (PAC-2)"

if ((VERBOSE)); then
	echo "+ $DOCKER inspect $CONTAINER" >&2
	$DOCKER inspect "$CONTAINER" >&2 || true
fi

echo "$SCRIPT_NAME: PASS — $IMAGE served, authenticated, persisted and stopped as claimed"
exit 0
