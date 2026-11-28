# Audit: REST facade remainder for WeftDB's HTTP API (#360)

**Date:** 2026-11-22. **Branch:** `b-rest/facade-audit` at commit `5294136a7` (origin/main).

## Question

`ROADMAP.md:194` reads `- [ ] **B-rest** REST facade remainder` under Phase 22 — Connectors and ingest interop. `docs/roadmap-notes.md:449` lists it as `- **B-rest** *(Phase 2 · partial)*`. Neither says what "remainder" means — which WeftDB capabilities are reachable over the HTTP API and which are reachable only through the library, offline tools, or a CLI.

## Method

### What "REST facade" means here

`weft-server/src/storage.rs:427` uses the phrase "params over the REST facade", referring to the whole WeftDB HTTP API (query params on a read endpoint). No file under `weft-server/src/` uses "facade" to mean a vendor-compatible shim, and `docs/reference.md` consistently calls the HTTP surface "HTTP API". Under the Phase 22 heading ("Connectors and ingest interop") the term could also describe an InfluxDB-compatible `/query`+`/write` facade (the line sits among connector items), but no evidence in the codebase or in `docs/roadmap-notes.md` uses "facade" for that meaning. This audit describes the remainder under **both** readings but treats WeftDB's own HTTP API as the primary one.

### Enumeration

```
# Route inventory: every registration call in the route registry
grep -n 'register_\|\.route(' weft-server/src/routes.rs

# All registration functions at 805-975
cat -n weft-server/src/routes.rs | sed -n '805,975p'

# Public pub async fn declarations in the library SegmentStore
grep -rn 'pub async fn' weftdb/src/types/segment_store/ --include='*.rs'

# Public fn declarations in tools/
ls tools/

# OpenAPI frozen path inventory
cat compat/openapi/weftdb.json | python3 -c \
  "import json,sys; d=json.load(sys.stdin); print('\n'.join(sorted(d.get('paths',{}).keys())))"
```

The route registry (`weft-server/src/routes.rs:656-665`) calls 11 registration functions covering 10 scopes. The OpenAPI baseline (`compat/openapi/weftdb.json`) lists the same paths. The library's `SegmentStore` (`weftdb/src/types/segment_store/`) has four submodules (`open.rs`, `read.rs`, `seal.rs`, `maint.rs`, `stats.rs`) plus the root `segment_store.rs` with lifecycle/fsck/quarantine/headroom methods.

## What the facade covers

All routes are stable (`RouteSpec::stable`) unless noted. 74 routes total (inventory below). Scopes listed follow the frozen scope list (`pub enum Scope` at `weft-server/src/routes.rs:82`, `Scope::ALL` at `:995`).

### Probe (routes.rs:805-808)
`GET /health`, `GET /healthz`, `GET /ready`, `GET /readyz`

### Metrics (routes.rs:859-864)
`GET /metrics`, `GET /debug/profile/current`, `GET /api/v1/storage/headroom` (experimental)

### Version (routes.rs:852-853)
`GET /api/v1/version`

### Read — storage catalog and metadata (routes.rs:909-941)
`GET /api/v1/storage/aspects`, `GET /api/v1/storage/catalog`, `GET /api/v1/storage/stats`,
`GET /api/v1/storage/{aspect}/schema`, `GET /api/v1/storage/{aspect}/stats`

### Read — time-range queries (routes.rs:913-926)
`GET /api/v1/storage/{aspect}/range` (JSON), `.range.parquet`, `.range.csv`,
`GET /api/v1/storage/{aspect}/points`,
`GET /api/v1/storage/{aspect}/downsample` (JSON, GET+POST), `.downsample.csv`, `.downsample.arrow`, `.downsample.parquet`,
`GET /api/v1/storage/{aspect}/interpolate` (JSON), `.interpolate.csv`, `.interpolate.arrow`, `.interpolate.parquet`

### Read — point and value-band queries (routes.rs:928-941)
`GET /api/v1/storage/{aspect}/at`, `GET /api/v1/storage/{aspect}/at-multi`,
`GET /api/v1/storage/{aspect}/value-range` (JSON), `.value-range.parquet`, `.value-range.csv`,
`GET /api/v1/storage/{aspect}/value-points`

### Compute — stateless interpolation and downsampling (routes.rs:886-903)
`POST /api/v1/interpolate` (JSON), `/interpolate/csv`, `arrow`, `parquet`, `/interpolate/ilp`, `/interpolate/ilp/csv`, `arrow`, `parquet`, `/interpolate/point`,
`POST /api/v1/downsample` (JSON), `/downsample/csv`, `arrow`, `parquet`, `/downsample/ilp`, `/downsample/ilp/csv`, `arrow`, `parquet`

### Ingest (routes.rs:944-951)
`POST /api/v1/storage/{aspect}/points` (JSON), `/{aspect}/ilp`, `/{aspect}/csv`, `/{aspect}/parquet`

### Declare (routes.rs:954)
`POST /api/v1/storage/aspects`

### Maintain — reconciliation and compaction (routes.rs:962-973)
`POST /api/v1/storage/reconcile`, `POST /api/v1/storage/{aspect}/reconcile`,
`POST /api/v1/storage/{aspect}/squash`,
`POST /api/v1/storage/{aspect}/compact`,
`POST /api/v1/storage/fsck`
`GET /api/v1/storage/quarantine`,
`POST /api/v1/storage/quarantine/purge-expired`,
`POST /api/v1/storage/quarantine/{aspect}/{id}/{gen}/release`,
`POST /api/v1/storage/quarantine/{aspect}/{id}/{gen}/purge`

### Backup (routes.rs:975-979)
`POST /api/v1/storage/backup`, `POST /api/v1/storage/restore/drill`, `POST /api/v1/storage/restore/into`

### Bench runs — experimental (routes.rs:871-873)
`GET /bench/runs`, `GET /bench/runs/{id}`

### Grafana — experimental (routes.rs:878-881)
`GET /api/v1/grafana`, `POST /api/v1/grafana/metrics`, `POST /api/v1/grafana/query`

## The remainder

Each capability below lives in `weftdb` and/or the offline CLI but has **no HTTP route**. A "no" in the last column means the item deliberately does not need a route; otherwise it is a gap.

| Library capability | HTTP route? | Why it matters | File issue? |
|---|---|---|---|
| `SegmentStore::expiry_sweep`, `purge_expired` (`maint.rs:22,82`) | None | Row TTL is user-facing data retention; a store with no daemon cannot trigger expiry at all. | Yes |
| `SegmentStore::changes_since`, `ack_changes` (`seal.rs:910,925`) | None | Change notification stream. Relevant to B-conncache (#871–#873); already tracked. | No (cite #871) |
| `SegmentStore::split_segment` (`maint.rs:983`) | None | Manual segment split at a wall-clock boundary. Called internally by `reconcile_overlaps`. | Yes |
| `SegmentStore::rebuild_aspect_metadata`, `rebuild_all_metadata`, `backfill_missing_rollups` (`maint.rs:2231,2250,2277`) | None | Metadata repair and rollup backfill: an operator cannot repair a corrupted metadata entry through the API. | Yes |
| `SegmentStore::resolve_series`, `series_tags` (`read.rs:681,712`) | None | Series resolution by selector and tag lookup. TAG-10 added series-aware read variants to the library but no HTTP route uses them. | Yes |
| `SegmentStore::declare_with_upsert_keys` (`open.rs:676`) | None | `POST /api/v1/storage/aspects` always uses default upsert keys. Minor. | No |
| `interpolate_range_f64` (`read.rs`, called from `stored_interpolate.rs:198`) | Not as f64 | HTTP stored interpolation uses BigDecimal. f64 is a library-only optimization. Already documented in `docs/reference.md`. | No |
| `SegmentStore::reap`, `reap_with_cancel` (`maint.rs:534,548`) | None | Segment deletion. Deliberately daemon-only: manual deletion could corrupt the index. | No (deliberate) |
| `wait_until_poisoned` (`open.rs:390`), `mark_clean_shutdown`, `shutdown` (`segment_store.rs:2182,2206`) | None | Process lifecycle. Deliberately daemon-only. C10 poison is exposed via `/ready`. | No (deliberate) |
| Dictionary operations (`new_dictionary`, `set_dictionary_metadata`, `insert_pattern_into_dictionary`, ...) | None | Pipeline-analytics feature. Not related to the core time-series REST surface. | No (deliberate) |

### Parsed under the "vendor-compatible connector" reading

If "REST facade" in the Phase 22 context means an InfluxDB-compatible `/query`+`/write` shim (like the connector items it sits among), then the gap is a different set: no route speaks the InfluxDB v1 `/query` or `/write` protocols (the `/ilp` ingest routes speak InfluxDB Line Protocol on the wire but do not implement the query protocol), and no route implements Prometheus remote write/read. The Prometheus remote write/read item is already an explicit sub-bullet on the same roadmap line at `ROADMAP.md:198`. No evidence in the codebase uses "facade" for this meaning; the InfluxDB ILP routes are the only concession to a vendor protocol.

## Follow-ups filed

| Issue | Summary |
|---|---|
| #878 | `POST /api/v1/storage/{aspect}/expire` — Row TTL expiry endpoint |
| #879 | `POST /api/v1/storage/{aspect}/split` — Manual segment split endpoint |
| #880 | `POST /api/v1/storage/metadata/rebuild` and `backfill-rollups` — Metadata repair routes |
| #881 | `GET /api/v1/storage/{aspect}/series` — Series resolution and tag lookup endpoint |

## Related but not re-filed

`changes_since`/`ack_changes` are covered by B-conncache (#871‑#873). `declare_with_upsert_keys` is a minor library-only variant of the existing declare endpoint. `interpolate_range_f64` is a precision trade-off already documented.

## What does not need a route

- **Segment deletion** (`reap`): deliberate daemon-only operation; manual deletion could corrupt the segment index.
- **Process lifecycle** (`wait_until_poisoned`, `mark_clean_shutdown`, `shutdown`): daemon lifecycle; poison state is already on `/ready`.
- **Dictionary operations**: pipeline-analytics feature outside the core time-series REST surface.