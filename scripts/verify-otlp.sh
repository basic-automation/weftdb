#!/usr/bin/env bash
# Verify OTLP trace delivery end-to-end: dsp-server -> OTLP/gRPC -> Jaeger.
#
# Ensures a local Jaeger all-in-one collector is running (starts the `jaeger`
# docker container if needed), boots dsp-server with OTEL_EXPORTER_OTLP_ENDPOINT
# set, drives a declare/ingest/read cycle against the storage API, then asserts
# via the Jaeger query API that a `request` root span with nested storage stage
# spans (`storage.ingest.parse`, `storage.range.read`, ...) actually landed.
#
# Requirements: docker, curl, python, cargo (bash via Git Bash on Windows).
# Usage: scripts/verify-otlp.sh   (exit 0 = delivery verified)
set -euo pipefail

JAEGER_UI=http://localhost:16686
OTLP_ENDPOINT=http://localhost:4317
SERVER_ADDR=127.0.0.1:8093
BASE="http://$SERVER_ADDR/api/v1"
ASPECT="otel_verify_$$"
STORE_ROOT="$(mktemp -d)"
SERVER_LOG="$STORE_ROOT/dsp-server.log"
SERVER_PID=""

cleanup() {
	[[ -n $SERVER_PID ]] && kill "$SERVER_PID" 2>/dev/null || true
	rm -rf "$STORE_ROOT" 2>/dev/null || true
}
trap cleanup EXIT

# --- 1. Collector: reuse a healthy Jaeger, or (re)start the container ---------
if ! curl -sf -o /dev/null "$JAEGER_UI/api/services"; then
	echo "jaeger query API not responding; starting the container..."
	docker start jaeger 2>/dev/null || docker run -d --name jaeger --restart unless-stopped \
		-p 16686:16686 -p 4317:4317 -p 4318:4318 jaegertracing/jaeger:latest
	for _ in $(seq 1 30); do
		curl -sf -o /dev/null "$JAEGER_UI/api/services" && break
		sleep 1
	done
	curl -sf -o /dev/null "$JAEGER_UI/api/services" || {
		echo "FAIL: jaeger did not become ready on $JAEGER_UI" >&2
		exit 1
	}
fi
echo "collector ready on $JAEGER_UI (OTLP/gRPC on $OTLP_ENDPOINT)"

# --- 2. Boot dsp-server with the exporter enabled -----------------------------
cargo build -p dsp-server
target_dir=$(cargo metadata --format-version 1 --no-deps |
	python -c "import json,sys; print(json.load(sys.stdin)['target_directory'])")
OTEL_EXPORTER_OTLP_ENDPOINT="$OTLP_ENDPOINT" \
	DSP_SERVER_ADDR="$SERVER_ADDR" \
	DSP_SEGMENT_STORE_ROOT="$STORE_ROOT/store" \
	"$target_dir/debug/dsp-server" >"$SERVER_LOG" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 30); do
	curl -sf -o /dev/null "http://$SERVER_ADDR/health" && break
	sleep 1
done
curl -sf -o /dev/null "http://$SERVER_ADDR/health" || {
	echo "FAIL: dsp-server did not become ready" >&2
	cat "$SERVER_LOG" >&2
	exit 1
}
grep -q "otlp trace export: enabled" "$SERVER_LOG" || {
	echo "FAIL: server booted without the OTLP exporter" >&2
	exit 1
}
echo "dsp-server up on $SERVER_ADDR with OTLP export enabled"

# --- 3. Drive traced requests (declare -> ingest -> range read) ---------------
curl -sf -X POST "$BASE/storage/aspects" -H 'Content-Type: application/json' \
	-d "{\"name\":\"$ASPECT\",\"physical_type\":\"f64\",\"timestamp_unit\":\"millis\"}" >/dev/null
curl -sf -X POST "$BASE/storage/$ASPECT/points" -H 'Content-Type: application/json' \
	-d '{"points":[[1000,"1.5"],[2000,"2.5"],[3000,"3.5"],[4000,"4.5"]]}' >/dev/null
curl -sf "$BASE/storage/$ASPECT/points?start=0&end=10000" >/dev/null
echo "traced declare/ingest/read cycle complete for aspect $ASPECT"

# --- 4. Assert the trace landed in Jaeger -------------------------------------
sleep 8 # batch exporter flush interval is ~5s
curl -s "$JAEGER_UI/api/traces?service=dsp-server&limit=20" | python -c "
import json, sys
data = json.load(sys.stdin).get('data') or []
ok = False
for trace in data:
    spans = trace['spans']
    roots = [s for s in spans if not s['references']]
    names = {s['operationName'] for s in spans}
    stages = {n for n in names if n.startswith('storage.')}
    if any(r['operationName'] == 'request' for r in roots) and stages:
        print(f\"OK: trace {trace['traceID']}: request root + nested stage spans {sorted(stages)}\")
        ok = True
sys.exit(0 if ok else 1)
" || {
	echo "FAIL: no request trace with nested storage stage spans found in Jaeger" >&2
	exit 1
}
echo "PASS: OTLP delivery verified (dsp-server -> $OTLP_ENDPOINT -> Jaeger)"
