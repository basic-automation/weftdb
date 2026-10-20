#!/usr/bin/env bash
# Regenerate compat/fixtures/v0.1.0 — the trees the first public release wrote
# (1.0 plan, slice X-FX).
#
# Every tree here is written by a *v0.1.0* binary: a v0.1.0 server ingests the shared
# value grid three ways into a Storage-v2 root, reconciles it, reads it back and takes a
# backup; the legacy generator (compat/v010/weftdb/examples/, compiled from the tag)
# writes a rows-mode database from the same grid. Reading them with the build under test
# is what `weftdb/tests/compat_v010.rs` gates.
#
# Usage:
#   scripts/compat/make-v010-fixtures.sh --bin-dir DIR [--out DIR] [--keep-work]
#
# --bin-dir must hold the v0.1.0 `weft-server` and `gen_legacy_fixture`. Build them from
# a worktree at the tag:
#
#   git worktree add -f /tmp/weft-v010 v0.1.0
#   cp compat/v010/weftdb/examples/gen_legacy_fixture.rs /tmp/weft-v010/weftdb/examples/
#   (cd /tmp/weft-v010 && cargo build --locked --release -p weftdb --example gen_legacy_fixture -p weft-server)
#
# The release profile is what the published binaries used; a debug build produces the
# same bytes on disk, but slower, and the fixture README records which was used.
set -euo pipefail

BIN_DIR=""
OUT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/compat/fixtures/v0.1.0"
REPO="${OUT%/compat/fixtures/v0.1.0}"
KEEP_WORK=0
SERVER_ADDR="127.0.0.1:18099"

while [[ $# -gt 0 ]]; do
	case "$1" in
	--bin-dir) BIN_DIR="$2"; shift 2 ;;
	--out) OUT="$2"; shift 2 ;;
	--keep-work) KEEP_WORK=1; shift ;;
	--help | -h)
		sed -n '2,22p' "${BASH_SOURCE[0]}"
		exit 0
		;;
	*)
		echo "unknown argument: $1" >&2
		exit 2
		;;
	esac
done

if [[ -z "$BIN_DIR" ]]; then
	echo "--bin-dir is required (a directory holding weft-server and gen_legacy_fixture built from v0.1.0)" >&2
	exit 2
fi
BIN_DIR="$(cd "$BIN_DIR" && pwd)"
# cargo leaves examples one level down from the binaries; accept either layout, and
# resolve each tool to a real path so the recorded digests name what actually ran.
declare -A TOOL=()
for bin in weft-server gen_legacy_fixture; do
	for candidate in "$BIN_DIR/$bin" "$BIN_DIR/examples/$bin"; do
		if [[ -x "$candidate" ]]; then
			TOOL[$bin]="$candidate"
			break
		fi
	done
	if [[ -z "${TOOL[$bin]:-}" ]]; then
		echo "--bin-dir/$bin is missing or not executable (a cargo build puts examples under --bin-dir/examples/)" >&2
		exit 2
	fi
done

WORK="$(mktemp -d -t weft-v010-fixtures.XXXXXX)"
SERVER_PID=""
cleanup() {
	if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
		kill "$SERVER_PID" 2>/dev/null || true
		wait "$SERVER_PID" 2>/dev/null || true
	fi
	if [[ "$KEEP_WORK" == 1 ]]; then
		echo "work tree kept at $WORK" >&2
	else
		rm -rf "$WORK"
	fi
}
trap cleanup EXIT

fail() {
	echo "make-v010-fixtures: $*" >&2
	exit 1
}

# The shared grid: subject,aspect,timestamp,value. One sawtooth per aspect, deterministic
# from the awk source alone, and values with at most 6 fractional digits so a scaled_i64
# encoding is exact and the same text can be read from either storage mode. Empty values
# (lines ending in a bare comma) become null rows, so the fixture carries the quality mask
# too — on the scaled aspect as well as the f64 one. The four appended rows sit far outside
# every series window: two at the epoch, one near the 32-bit ceiling and one at a
# second-timestamp in the 23rd century, which is what a seconds-scale store can hold.
cat > "$WORK/grid.awk" <<'AWK'
BEGIN {
	start = 1767225600 # 2026-01-01T00:00:00Z
	for (i = 0; i < 240; i++) {
		if (i % 37 == 11) printf "plant,price,%d,\n", start + i * 60
		else printf "plant,price,%d,%.4f\n", start + i * 60, 100.25 + 0.75 * (((i * 7) % 11) / 3)
	}
	for (i = 0; i < 60; i++) {
		ts = start + i * 120
		if (i % 10 == 7) printf "plant,temp,%d,\n", ts
		else printf "plant,temp,%d,%.4f\n", ts, 21.5 + 1.5 * (((i * 7) % 11) / 3)
	}
	# Two disjoint bursts with a hole between them, so a reader has to cross a gap.
	for (i = 0; i < 40; i++) printf "plant,flow,%d,%d\n", start + i * 60, i
	for (i = 0; i < 40; i++) printf "plant,flow,%d,%d\n", start + 3600 + i * 60, 100 + i
	# Outside every window a fixture test would ask with, and at the edges a second-scale
	# timestamp can reach.
	printf "plant,price,0,103.5000\n"
	printf "plant,price,1,104.5000\n"
	printf "plant,price,2147483648,105.5000\n"   # one past the 32-bit ceiling
	printf "plant,price,4070900000,106.5000\n"   # 2099-02-14, still seconds
}
AWK
awk -f "$WORK/grid.awk" > "$WORK/rows.csv"
grep -E '^plant,price,' "$WORK/rows.csv" | cut -d, -f3,4 > "$WORK/price.csv"
grep -E '^plant,temp,' "$WORK/rows.csv" | cut -d, -f3,4 > "$WORK/temp.csv"
grep -E '^plant,flow,' "$WORK/rows.csv" | head -40 | cut -d, -f3,4 > "$WORK/flow-a.csv"
grep -E '^plant,flow,' "$WORK/rows.csv" | tail -40 | cut -d, -f3,4 > "$WORK/flow-b.csv"

# JSON points: the same rows as `points:[{timestamp,value}]`, values as decimal strings.
python3 - "$WORK/rows.csv" "$WORK/points.json" <<'PY'
import json, sys
points = []
for line in open(sys.argv[1]):
	subject, aspect, ts, value = line.strip().split(",")
	if subject != "plant" or aspect != "price":
		continue
	points.append({"timestamp": int(ts), "value": value if value else None})
json.dump({"points": points}, open(sys.argv[2], "w"))
PY

# ILP: same price rows again, this time as line protocol with a second-precision stamp.
# Line protocol has no way to say "null", so this leg carries the non-null subset — the
# other two legs are what put nulls in the store, which is what the recorded reads show.
python3 - "$WORK/rows.csv" "$WORK/price.ilp" <<'PY'
import sys
out = []
for line in open(sys.argv[1]):
	subject, aspect, ts, value = line.strip().split(",")
	if subject != "plant" or aspect != "price" or not value:
		continue
	out.append(f"weftdb,aspect={aspect} value={value} {ts}")
open(sys.argv[2], "w").write("\n".join(out) + "\n")
PY

SERVER_LOG="$WORK/server.log"

start_server() { # $1: store root
	mkdir -p "$1"
	WEFT_SEGMENT_STORE_ROOT="$1" WEFT_BACKUP_DIR="$WORK/backups" WEFT_SERVER_ADDR="$SERVER_ADDR" \
		"${TOOL[weft-server]}" > "$SERVER_LOG" 2>&1 &
	SERVER_PID=$!
	for _ in $(seq 1 120); do
		if curl -fsS -m 2 "http://$SERVER_ADDR/health" > /dev/null 2>&1; then
			return 0
		fi
		if ! kill -0 "$SERVER_PID" 2>/dev/null; then
			tail -20 "$SERVER_LOG" >&2
			fail "the v0.1.0 server exited before becoming healthy"
		fi
		sleep 0.5
	done
	tail -20 "$SERVER_LOG" >&2
	fail "the v0.1.0 server never answered /health on $SERVER_ADDR"
}

api() { # method path [data-file]
	local method="$1" path="$2" data="${3:-}"
	if [[ -n "$data" ]]; then
		curl -fsS -m 30 -X "$method" -H 'content-type: application/json' --data-binary @"$data" "http://$SERVER_ADDR$path"
	else
		curl -fsS -m 30 -X "$method" "http://$SERVER_ADDR$path"
	fi
}

# ── The Storage-v2 tree: a v0.1.0 server, a temp root, three ingest formats ─────────
declare_body() {
	cat > "$WORK/declare-$1.json" <<JSON
{"name":"$1","physical_type":"$2","timestamp_unit":"seconds"$3}
JSON
}
declare_body price scaled_i64 ',"scale":4'
declare_body temp f64 ''
declare_body flow f64 ''

start_server "$WORK/store"
for aspect in price temp flow; do
	response="$(api POST /api/v1/storage/aspects "$WORK/declare-$aspect.json")"
	grep -q "\"name\":\"$aspect\"" <<< "$response" || fail "declaring $aspect returned $response"
done

# CSV, then JSON points, then ILP: the same price rows through every ingest path, so the
# fixture's segments cover each seal route that existed at the tag.
ingest_csv() { # aspect file
	local response
	response="$(curl -fsS -m 60 -X POST --data-binary @"$2" "http://$SERVER_ADDR/api/v1/storage/$1/csv")"
	grep -q '"row_count"' <<< "$response" || fail "csv ingest for $1 returned $response"
}
ingest_csv price "$WORK/price.csv"
ingest_csv temp "$WORK/temp.csv"
ingest_csv flow "$WORK/flow-a.csv"
ingest_csv flow "$WORK/flow-b.csv"
response="$(api POST '/api/v1/storage/price/points?require_sorted=true' "$WORK/points.json")"
# The JSON leg must carry the same 244 price rows the grid holds — and the same 7 nulls,
# which is how a null survives ingest into a scaled aspect at this version.
if ! { grep -q '"row_count":244' <<< "$response" && grep -q '"null_count":7' <<< "$response"; }; then fail "json ingest returned $response"; fi
response="$(api POST '/api/v1/storage/price/ilp?precision=s&field=value' "$WORK/price.ilp")"
# 237, not 244: the seven nulls the other legs carry have no ILP spelling.
grep -q '"row_count":237' <<< "$response" || fail "ilp ingest returned $response"

# One deliberately out-of-order batch, so the tree also holds an unsorted segment and the
# reconciliation pass has something real to rewrite. `require_sorted` is off by default at
# this version precisely because out-of-order data is the case the reader must survive.
tac "$WORK/temp.csv" | head -12 > "$WORK/temp-unsorted.csv"
ingest_csv temp "$WORK/temp-unsorted.csv"

# Reconcile so the out-of-order batch is rewritten into a time-sorted segment: the fixture
# must contain both shapes, because those are the two a reader can meet in the wild.
response="$(api POST /api/v1/storage/temp/reconcile)"
grep -q '"reconciled"' <<< "$response" || fail "per-aspect reconcile returned $response"
grep -qE '"reconciled":[1-9]' <<< "$response" || fail "the out-of-order batch was not reconciled: $response"
# Fold `flow`'s two bursts into one segment, so the tree also holds a squashed aspect —
# the other shape a reader can meet. price and temp are deliberately left as they were
# sealed (three overlapping batches, one sorted and one reconciled), because those are the
# hard cases for a reader and the easy case is already covered here.
response="$(api POST /api/v1/storage/flow/squash)"
grep -q '"segment_count":1' <<< "$response" || fail "squash of flow returned $response"

stats_before="$(api GET /api/v1/storage/stats)"

# The reads are recorded while a v0.1.0 server is still the one answering them. The
# compat test re-runs the same ranges with the build under test and compares the text.
: > "$WORK/expected_reads.txt"
record_read() { # kind aspect query
	local kind="$1" aspect="$2" query="$3"
	printf '# %s %s %s\n' "$kind" "$aspect" "$query" >> "$WORK/expected_reads.txt"
	curl -fsS -m 30 "http://$SERVER_ADDR/api/v1/storage/$2/$1?$3" | sed 's/\r$//' >> "$WORK/expected_reads.txt"
}
record_read range.csv 'price' 'start=1767225600&end=1767239999'
record_read range.csv 'temp' 'start=1767225600&end=1767232999'
record_read range.csv 'price' 'start=1767240000&end=1767250000'
record_read range.csv 'flow' 'start=1767225600&end=1767232800'
record_read value-range.csv 'price' 'lo=100.5&hi=102.5'
{
	printf '# at price t=1767225660\n'
	curl -fsS -m 30 "http://$SERVER_ADDR/api/v1/storage/price/at?t=1767225660"
	printf '\n'
	printf '# downsample.csv price start=1767225600&end=1767239999&resolution=minutes&agg=min,max,avg,twa\n'
	curl -fsS -m 30 "http://$SERVER_ADDR/api/v1/storage/price/downsample.csv?start=1767225600&end=1767239999&resolution=minutes&agg=min,max,avg,twa"
	printf '\n'
} >> "$WORK/expected_reads.txt"

{
	for aspect in price temp flow; do
		printf '# stats %s\n' "$aspect"
		api GET "/api/v1/storage/$aspect/stats"
		printf '\n'
	done
} >> "$WORK/aspect_stats.txt"

api GET /api/v1/storage/catalog > "$WORK/catalog.json"
api GET /api/v1/storage/aspects > "$WORK/aspects.json"

# The backup archive, taken by the same server that owns the store.
response="$(api POST /api/v1/storage/backup)"
grep -q '"dir"' <<< "$response" || fail "backup returned $response"
printf '%s\n' "$response" | python3 -m json.tool > "$WORK/backup.json"
label="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["dir"].rsplit("/",1)[-1])' "$WORK/backup.json")"
drill="$(api POST "/api/v1/storage/restore/drill?label=$label")"
grep -q '"label"' <<< "$drill" || fail "restore drill of $label returned $drill"
printf '%s\n' "$drill" | python3 -m json.tool > "$WORK/restore_drill.json"
# Two things in those reports are machine-made rather than data: the backup label is a
# wall-clock stamp the tag generates itself, and the paths name this run's temp directory.
# Redact both so a regenerated fixture diffs clean against what is checked in. No other
# byte in any record is touched, and the README says so.
sed -i -e "s#$WORK#<work>#g" -e 's#manual-[0-9]\{6,\}#manual-<timestamp>#g' "$WORK/backup.json" "$WORK/restore_drill.json"

kill "$SERVER_PID"
wait "$SERVER_PID" 2>/dev/null || true
SERVER_PID=""
printf 'server exited cleanly\n' >> "$SERVER_LOG"

# A v0.1.0 store root is four turso control databases plus `segments/<aspect>-<id>.weftseg`.
# There is no `store.json` marker at this version — the marker postdates the tag — and no
# `segment_manifest.json`: the segment descriptors live in `segment_index.db`. Refuse a
# tree that is missing either half, because a fixture that silently holds no segment is
# worse than no fixture.
for required in catalog.db aspect_catalog.db metadata.db segment_index.db segments; do
	[[ -e "$WORK/store/$required" ]] || fail "the v0.1.0 store holds no $required"
done
segment_count="$(find "$WORK/store/segments" -name '*.weftseg' | wc -l)"
[[ "$segment_count" -ge 4 ]] || fail "the v0.1.0 store sealed only $segment_count segment(s); expected at least 4"

# ── The legacy rows-mode tree: the same grid through the v0.1.0 library ─────────────
mkdir -p "$WORK/legacy"
WEFT_DATA_DIR="$WORK/legacy" "${TOOL[gen_legacy_fixture]}" v010_legacy "$WORK/rows.csv" > "$WORK/legacy.log" 2>&1 || fail "the legacy generator failed: $(tail -5 "$WORK/legacy.log")"
[[ -e "$WORK/legacy/v010_legacy/metadata.db" ]] || fail "the legacy tree has no metadata.db"

# ── Publish the trees ───────────────────────────────────────────────────────────────
rm -rf "$OUT"
mkdir -p "$OUT"
cp -a "$WORK/store" "$OUT/store"
cp -a "$WORK/backups" "$OUT/backup"
cp -a "$WORK/legacy" "$OUT/legacy"
cp "$WORK/expected_reads.txt" "$OUT/expected_reads.txt"
cp "$WORK/aspect_stats.txt" "$OUT/aspect_stats.txt"
cp "$WORK/catalog.json" "$OUT/catalog.json"
cp "$WORK/aspects.json" "$OUT/aspects.json"
cp "$WORK/backup.json" "$OUT/backup.json"
cp "$WORK/restore_drill.json" "$OUT/restore_drill.json"
cp "$WORK/rows.csv" "$OUT/rows.csv"
( cd "$OUT" && find store -type f -printf '%P\t%s\n' | sort > segment_listing.txt )
printf '%s\n' "$stats_before" | python3 -m json.tool > "$OUT/stats.json"

sha256sum "${TOOL[weft-server]}" "${TOOL[gen_legacy_fixture]}" > "$OUT/binaries.sha256"
# A v0.1.0 weft-server has no `--version`: give it one and it starts listening instead
# (and fails with `Address already in use` if one is already up). So provenance is the tag,
# the commit the tag's annotated object peels to, and the digest of what actually ran.
{
	printf 'tag\tv0.1.0\n'
	printf 'commit\t%s\n' "$(git -C "$REPO" rev-parse 'v0.1.0^{commit}')"
	printf 'tag_object\t%s\n' "$(git -C "$REPO" rev-parse v0.1.0)"
	printf 'weft-server\t%s\n' "${TOOL[weft-server]}"
	printf 'gen_legacy_fixture\t%s\n' "${TOOL[gen_legacy_fixture]}"
	printf 'rustc\t%s\n' "$(rustc --version 2>/dev/null || echo unknown)"
	printf 'note\tthis version has no --version flag; passing one starts a server\n'
} > "$OUT/provenance.txt"

cat > "$OUT/README.md" <<'MD'
# `compat/fixtures/v0.1.0` — what the first public release wrote

Every byte under `store/`, `backup/` and `legacy/` was written by a **v0.1.0** binary built
from the `v0.1.0` tag, and is committed unchanged. `weftdb/tests/compat_v010.rs` opens them
with the build under test; that is the upgrade/rollback evidence for 1.0 plan slice X-FX.
`MANIFEST.sha256` holds one sha256 per file in the set — the three trees and every record
(109 files as committed),
`binaries.sha256` the digest of the two binaries that wrote them, and `provenance.txt` the
tag, the commit its annotated object peels to, and the toolchain. A v0.1.0 `weft-server`
has **no `--version` flag** — passing one starts a server — so the digest is how these trees
say what produced them.

Regenerate with:

```bash
scripts/compat/make-v010-fixtures.sh --bin-dir <dir with weft-server + gen_legacy_fixture built from v0.1.0>
```

## The three trees

- **`store/` — a Storage-v2 root as the tag leaves it.** Three aspects off one grid:
  `price` (declared `scaled-i64:4`, seconds, exact tolerance), `temp` and `flow` (f64,
  seconds). It arrived through three different ingest routes — CSV body for all three,
  JSON `points` for `price` (deliberately unsorted), ILP line protocol for `price` again
  (237 of its 244 rows: line protocol has no spelling for null, so the nulls entered by
  the other two routes). The aspects are left in the three shapes a reader can meet:
  `price` as three **overlapping** batches (3 segments, 725 rows, 14 nulls, 2 of them
  unsorted), `temp` as a sorted batch plus a reversed batch **reconciled in place** (2
  segments, 72 rows, 7 nulls), `flow` **squashed** to a single segment (80 rows).
  There is **no `store.json` marker at this version** — the marker postdates the tag — and
  no `segment_manifest.json`: the descriptors live in `segment_index.db`, which is why a
  reader has to classify the tree from what is in it rather than from what it says.
- **`backup/` — what `POST /api/v1/storage/backup` wrote** while that server owned the
  store, with the server's own report (`backup.json`) and the **restore-drill report**
  (`restore_drill.json`). What this version actually backs up is the control plane —
  `catalog.db`, `aspect_catalog.db`, `metadata.db`, `segment_index.db` — and **not**
  `segments/`. A v0.1.0 backup therefore cannot restore an ingested store by itself, which
  is worth knowing before anything promises a restore path across versions.
- **`legacy/` — a rows-mode database**, not a Storage-v2 root: `v010_legacy/metadata.db`
  plus one directory per subject/aspect, each holding its own Turso `measurements.db`,
  `pipeline.db`, `correlations.db` and friends. Written by the `gen_legacy_fixture` example
  compiled from the tag, through the v0.1.0 library's own write path.

## The records

- **`rows.csv`** — the shared grid (384 rows): 244 `price` rows at minute steps, 7 of them
  null, plus four rows far outside every series window — two at the epoch, one at
  `2147483648` (one past the 32-bit ceiling) and one at `4070900000` — because a
  second-scale store must survive those and a v0.1.0 read is the only claim about whether it
  did; 60 `temp` rows at two-minute steps, 6 null; 80 `flow` rows in two bursts an hour
  apart. One grid, three ingest routes, one `expected_reads.txt` describing what each must
  answer.
- **`expected_reads.txt`** — one `# kind aspect query` header above each body: three
  `range.csv` windows, a `value-range.csv`, an `at`, and a `downsample.csv` bucket sweep,
  every one answered by a v0.1.0 server. `aspect_stats.txt` holds the three `/{aspect}/stats`
  bodies the same way; `catalog.json`, `aspects.json` and `stats.json` are the store's own
  account of itself at the tag, kept so a migration that changes the shape has to say so.

## What is not byte-stable, and what is

Databases are not byte-stable (Turso stamps write times inside them), which is why the trees
are verified by **opening** them and `MANIFEST.sha256` only freezes what is in them today.
Two JSON records would otherwise move on every regeneration for reasons that are not data:
the backup label (`manual-<wall-clock>`, which the tag generates itself) and this run's temp
directory in the paths. The generator redacts both to `manual-<timestamp>` and `<work>`; no
other byte in any record is touched, and the on-disk backup directory keeps the real label.
Everything else — `expected_reads.txt`, `aspect_stats.txt`, `segment_listing.txt`,
`catalog.json`, `aspects.json`, `stats.json` — has been checked to reproduce byte-for-byte
from the tag's binaries.

## What the test proves

1. `open_report` names this root's layout **before** any write — a v0.1.0 root carries no
   marker, so it is classified from its shape — and the current layout **after**, with the
   store's values unchanged across that migration.
2. The migrated store **reopens stably**: the same format version, the same reads.
3. The recorded `range.csv` reads answer the same timestamps, values **and nulls**, over the
   full range and the windowed range — and the declared `scaled-i64:4` / seconds / exact
   schema on `price` survives, so the values come back at scale, not as a guess.
4. The shapes are the shapes: 6 segments for 3 aspects, overlapping, reconciled and squashed.

The honest gap: **`legacy/` is not readable by this build.** Rows-mode v0.1.0 stores their
measurements in Turso databases, and the current engine never opens Turso — the roadmap's
D-S4 entry says as much ("Nothing in the repo reads it back"). So `legacy/` is checked in as
the evidence of what FRE-14's migration has to consume, and its test asserts the *shape* of
what FRE-14 must read, not a read. Rows-mode has no null values either, so that tree is the
non-null subset of `rows.csv`; the counts differ from the store's by design.
MD

# One digest per file in the whole set — the three trees and every record — as paths
# relative to the fixture root, so `python3 scripts/compat/releases.py verify`, which
# re-hashes every listed file and refuses anything unlisted, catches drift in a record as
# readily as in a database nobody can open by hand. This runs last, so the digest list
# covers the README and provenance it is written beside.
(
	cd "$OUT"
	find . -type f ! -name 'MANIFEST.sha256*' -printf '%P\0' | sort -z | xargs -0 sha256sum > MANIFEST.sha256.new
	mv MANIFEST.sha256.new MANIFEST.sha256
)

echo "wrote $OUT"
du -sh "$OUT"/* | sort -k2
