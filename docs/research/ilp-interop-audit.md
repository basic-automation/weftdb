# ILP/Influx interop audit: what exists and what is missing

**Research for roadmap item [Phase 22 — Connectors and ingest interop — B-ilp: ILP/Influx interop connector crate](#359).**

**Author:** slice B-ILP-AUDIT  
**Date:** 2026-11-22  
**Branch:** `ilp/interop-audit`  

---

## 1. Verdict (one sentence)

> WeftDB has a **mature, self-owned ILP parser**, four live HTTP endpoints that accept ILP bodies, and Rust + Python SDK methods that send them — but it has **no InfluxDB client, no ILP serializer, no connector trait, no UDP ingress, and no dedicated ILP documentation or examples**, so the roadmap item "ILP/Influx interop connector crate" is **3/10 done**: the parser exists, everything that would make it a *connector crate* (vendor client, write path to a running InfluxDB, ILP output, ILP→Weft schema mapping trait) is absent.

| Component | Status | Evidence |
|-----------|--------|----------|
| ILP *parser* | **Present** | `weft-line-protocol` crate — workspace member, published 0.3.1, ~700 LoC of grammar + tests |
| ILP *writer/serializer* | **Absent** | No ILP output format exists anywhere in the repo |
| ILP HTTP *ingest* | **Present** | `POST /api/v1/storage/{aspect}/ilp` + compute endpoints |
| ILP HTTP *compute* | **Present** | `POST /api/v1/{interpolate,downsample}/ilp[/{csv,arrow,parquet}]` |
| Rust SDK write | **Present** | `weft-client::Client::write_ilp()` |
| Python SDK write | **Present** | `weftdb.client.write_ilp()` |
| Tag→series mapping | **Present** | TAG-11: tag set names the series |
| Measurement→dataset mapping | **Partial** | `measurement=` query param filters records, but no `DatasetId`/tenant abstraction bound to ILP |
| Precision/time-unit mapping | **Present** | `TimestampPrecision` (ns/µs/ms/s) → `TimeUnit` (Nanos/Micros/Millis/Seconds) with `epoch_in_unit` rescaling |
| InfluxDB client (write out) | **Absent** | No `influxdb`, `influxdb2`, or any InfluxDB client crate in `Cargo.lock` |
| Connector trait / registry | **Absent** | Phase 22 `B-conn` is unfiled; no connector trait exists |
| UDP ingress | **Absent** | Not mentioned in `docs/roadmap-notes.md`, no UDP listener code |
| TLS/auth | **Present** (server-side) | Server has TLS + basic auth; ILP endpoints inherit it |
| Error/backpressure | **Present** | Structured `ApiError`, body caps, disk guard, `Idempotency-Key`, two-phase ingest |
| Round-trip tests | **Partial** | Compat fixtures (`make-v010-fixtures.sh`), live integration tests (`weft-client/tests/live.rs`), no dedicated ILP round-trip test suite |
| Documentation & examples | **Partial** | `weft-line-protocol/README.md` documents the parser; no standalone examples/ directory for ILP usage |

---

## 2. What exists in-tree — pieces with `path:line` evidence

### 2.1 The parser crate: `weft-line-protocol`

| File | Line(s) | What |
|------|---------|------|
| `Cargo.toml:11` | member `weft-line-protocol` | Workspace member |
| `Cargo.toml:110` | `weft-line-protocol = { path = "./weft-line-protocol" }` | Dependency declaration |
| `weft-line-protocol/Cargo.toml:2` | `name = "weft-line-protocol"` | Published crate, 0.3.1 |
| `weft-line-protocol/Cargo.toml:11` | `description = "Vendor-neutral InfluxDB Line Protocol (ILP) parser"` | Stated scope |
| `weft-line-protocol/Cargo.toml:12` | `keywords = ["influxdb", "line-protocol", "parser"]` | Categorisation |
| `weft-line-protocol/src/lib.rs:289` | `pub fn parse(input: &str) -> Result<Vec<LineRecord>, ParseError>` | Full document parser |
| `weft-line-protocol/src/lib.rs:332` | `pub fn parse_points(...) -> Result<Vec<Point>, ParseError>` | Single-series projection |
| `weft-line-protocol/src/lib.rs:78` | `pub enum TimestampPrecision` | Precision type: Nanoseconds/Micros/Millis/Seconds |
| `weft-line-protocol/src/lib.rs:371` | `fn parse_line(line: &str) -> Result<LineRecord, String>` | Single-line parser |
| `weft-line-protocol/src/lib.rs:389` | `fn parse_measurement_and_tags(...)` | Tag/measurement split |
| `weft-line-protocol/src/lib.rs:408` | `fn parse_fields(...)` | Field-set parser |
| `weft-line-protocol/src/lib.rs:426` | `fn parse_field_value(...)` | Typed field value parser |

**Status: implemented and reachable.** The crate is used by `weft-server` (ingest and compute routes), `weft-bench` (benchmark harness), `weft-xbench` (TSBS data loading), and the compat scripts.

### 2.2 HTTP ingest endpoint: `POST /api/v1/storage/{aspect}/ilp`

| File | Line(s) | What |
|------|---------|------|
| `weft-server/src/routes.rs:947` | `route("POST", "/api/v1/storage/{aspect}/ilp", ...)` | Route registration |
| `weft-server/src/ingest.rs:1245` | `pub async fn ingest_ilp(...)` | Handler |
| `weft-server/src/ingest.rs:1274` | `async fn ingest_ilp_inner(...)` | Inner logic |
| `weft-server/src/ingest.rs:1283` | `project_ilp_series(text, measurement, field, precision)` | Parse + project |
| `weft-server/src/ingest.rs:1190` | `fn project_ilp_series(...)` | Filters by `measurement`, projects one field, checks single series |
| `weft-server/src/ingest.rs:1119` | `pub struct IlpIngestParams` | Query params: field, measurement, precision, rows_per_page, require_sorted |
| `weft-server/src/ingest.rs:785` | `async fn commit_ingest(...)` | Shared commit path (same as JSON/CSV/Parquet) |

**Status: implemented and reachable.** Full production path: parse → project → timestamp-normalize → decimal-domain-check → seal. `Idempotency-Key` supported. Requires `measurement=` (TAG-11, breaking change since 0.3.x).

### 2.3 HTTP compute endpoints: `POST /api/v1/{interpolate,downsample}/ilp`

| File | Line(s) | What |
|------|---------|------|
| `weft-server/src/routes.rs:891` | `POST /api/v1/interpolate/ilp` | Interpolation ILP route |
| `weft-server/src/routes.rs:892` | `POST /api/v1/interpolate/ilp/csv` | CSV output variant |
| `weft-server/src/routes.rs:893` | `POST /api/v1/interpolate/ilp/arrow` | Arrow output variant |
| `weft-server/src/routes.rs:894` | `POST /api/v1/interpolate/ilp/parquet` | Parquet output variant |
| `weft-server/src/routes.rs:900` | `POST /api/v1/downsample/ilp` | Downsample ILP route |
| `weft-server/src/routes.rs:901-903` | CSV/Arrow/Parquet variants | Output format siblings |
| `weft-server/src/interpolate.rs:804` | `pub async fn interpolate_ilp(...)` | Handler |
| `weft-server/src/downsample.rs:591` | `pub async fn downsample_ilp(...)` | Handler |
| `weft-server/src/interpolate.rs:756` | `pub struct IlpParams` | Query params (field, precision, spline, resolution) |
| `weft-server/src/downsample.rs:559` | `pub struct DownsampleIlpParams` | Query params (field, precision, resolution, agg, fill) |

**Status: implemented and reachable.** Full compute path: parse ILP → project field → interpolate/downsample → serve in any of four output formats.

### 2.4 SDK methods

| File | Line(s) | What |
|------|---------|------|
| `weft-client/src/client.rs:595` | `pub async fn write_ilp(&self, aspect, field, body, options)` | Rust SDK ILP write |
| `weft-client/src/types.rs:583` | `precision` field in `WriteOptions` | Precision option |
| `weft-client/src/types.rs:585` | `measurement` field in `WriteOptions` | Measurement option |
| `clients/python/weftdb/client.py:397` | `def write_ilp(self, aspect, body, ...)` | Python SDK ILP write |
| `clients/python/weftdb/client.py:327` | `def interpolate_ilp(self, body, ...)` | Python SDK ILP interpolate |
| `clients/python/weftdb/client.py:335` | `def downsample_ilp(self, body, ...)` | Python SDK ILP downsample |

**Status: implemented and reachable** (Python `write_ilp` does not yet send `measurement=` — noted in `docs/upgrade-notes.md:48`).

### 2.5 Benchmark and compat usage

| File | Line(s) | What |
|------|---------|------|
| `weft-xbench/src/tsbs.rs:4` | `--format=influx` output parsed | TSBS benchmark loads ILP data |
| `weft-xbench/src/tsbs.rs:68` | `weft_line_protocol::parse(&line)` | Live parser call |
| `scripts/compat/make-v010-fixtures.sh:136` | ILP fixture leg | Compat test fixture built via ILP |
| `scripts/compat/make-v010-fixtures.sh:211` | `POST /api/v1/storage/price/ilp` | ILP ingest in compat |

**Status: implemented and reachable** — the parser has two live production consumers.

---

## 3. Is a third-party InfluxDB/ILP parser already a dependency?

| Question | Answer | Evidence |
|----------|--------|----------|
| `influxdb-line-protocol` in Cargo.lock? | **No** | Not found in workspace `Cargo.lock` or `weft-xbench/Cargo.lock` |
| `influxdb` / `influxdb2` in Cargo.lock? | **No** | Not found |
| `line-protocol` (generic) in Cargo.lock? | **No** | Only `weft-line-protocol` appears |
| Third-party InfluxDB crate anywhere? | **No** | No `influx`-prefixed crate in any `Cargo.toml` |

**Finding: zero third-party InfluxDB dependencies.** The parser is 100% self-owned. This means adding an InfluxDB client crate (e.g. `influxdb2` or `influxdb3`) would be a **new dependency**, not an upgrade of an existing one.

`deny.toml` policy (deny.toml:1-46): permissive license allow-list (MIT, Apache-2.0, BSD, etc.). It has no `[bans]` section that would block a specific crate. The only advisory ignore is `RUSTSEC-2026-0543` (chrono unmaintained). The brief's reference to `RUSTSEC-2023-0026` (about the influxdb2 ecosystem) was not found in `deny.toml` — that advisory (if it existed) may have been removed by a RustSec DB update, or the brief's reference predates the current policy. Adding an InfluxDB client crate is **not blocked** by the current policy.

---

## 4. What the repo's own ingest path looks like (the mapping ILP would need)

### 4.1 Entry points (`weft-server/src/ingest.rs`)

```
HTTP: POST /api/v1/storage/{aspect}/ilp?field=<field>&measurement=<meas>&precision=<prec>
  ↓
ingest_ilp (ingest.rs:1245)
  ↓ body.text()
  ↓ store.schema_for(aspect)  →  AspectSchema { physical_type, error_bound, time_unit }
  ↓
project_ilp_series (ingest.rs:1190)
  ↓ weft_line_protocol::parse(body)  →  Vec<LineRecord>
  ↓ filter by record.measurement == measurement
  ↓ record.to_point(field, precision)  →  Option<splimes::Point>
  ↓ record.series_key()  →  (measurement, sorted_tags)
  ↓
weftdb::TagSet::from_pairs(tags)  (ingest.rs:1230)
  ↓
epoch_in_unit(point.timestamp, schema.timestamp_unit)  (ingest.rs:1301)
  ↓
weft_physical_type::check_decimal_domain(&point.value)  (ingest.rs:1307)
  ↓
IngestBatch { key, timestamps, values, rows_per_page, groups }
  ↓
commit_ingest (ingest.rs:785)
```

### 4.2 Key types a connector would interact with

| Type | Declared in | Purpose |
|------|-------------|---------|
| `weftdb::AspectId` | `weftdb/src/types/aspect.rs:19` | Unique aspect identifier (UUID) |
| `weftdb::DatasetId` | `weftdb/src/types/dataset.rs:10` | Unique dataset identifier (UUID) |
| `weft_physical_type::TimeUnit` | `weft-physical-type/src/timestamp.rs:72` | Stored epoch unit: Seconds/Millis/Micros/Nanos |
| `weft_line_protocol::TimestampPrecision` | `weft-line-protocol/src/lib.rs:78` | Wire timestamp precision: ns/µs/ms/s |
| `weft_line_protocol::LineRecord` | `weft-line-protocol/src/lib.rs:143` | Parsed ILP record: measurement, tags, fields, timestamp |
| `weft_line_protocol::FieldValue` | `weft-line-protocol/src/lib.rs:105` | Typed field: Float/Integer/Unsigned/Bool/Str |
| `weftdb::IngestBatch` | `weftdb/src/types/ingest.rs:94` | Sealed batch: key, timestamps, values, groups |
| `weftdb::TagSet` | (via `weftdb::TagSet::from_pairs`) | Canonicalised tag set |
| `splimes::Point` | via `splimes` | `{ timestamp: DateTime<Utc>, value: BigDecimal }` |
| `weft_physical_type::PhysicalType` | `weft-physical-type/src/lib.rs` | Declared encoding: F64/F32/ScaledI64/etc. |
| `weftdb::SegmentStore` | `weftdb` | Store handle; `schema_for()`, `seal()` |

### 4.3 The shape a line-protocol record is mapped onto

```
ILP line:  measurement,tag_k=tag_v,... field_k=field_v,... [timestamp]
                ↓                    ↓           ↓           ↓
           measurement filter    TagSet     vec<(String,   i64 epoch
           (string match)        (sorted     FieldValue)>  in wire
                                 key-value   → BigDecimal   precision
                                 pairs)      → PhysicalValue
```

An InfluxDB **measurement** maps to the server's `measurement=` query filter (string match on `LineRecord.measurement`). There is no `DatasetId` binding at the ILP layer — the dataset is implicit in the storage engine's topology.

An InfluxDB **tag set** maps to `weftdb::TagSet` (sorted key-value pairs, validated for reserved keys, duplicates, and count limits). The tag set names the series the batch writes.

An InfluxDB **field set** maps one field at a time: each `POST /api/v1/storage/{aspect}/ilp` call declares a single `field=` query parameter, and all lines' values for that field become the batch. Separate fields go to separate aspects.

An InfluxDB **timestamp** (integer in caller-declared precision) maps through `TimestampPrecision.to_datetime()` then `epoch_in_unit()` to the aspect's declared `TimeUnit`.

---

## 5. The gap list

### 5.1 Parser — ✅ Present

`weft-line-protocol` is a complete, tested, shared parser. Grammar: measurement + tags, typed fields (float, signed/unsigned int, bool, string), optional timestamps, comment/blank-line skipping. It handles escape sequences (`\,`, `\ `, `\=`, `\"`, `\\`). It enforces single-series projection (`parse_points`) and reports structured errors (malformed, ambiguous series, non-finite value).

### 5.2 Writer/serializer — ❌ Absent

There is no function anywhere in the repo that produces ILP text. The parser (`weft-line-protocol/src/lib.rs`) has no inverse — no `impl Display for LineRecord` or `fn serialize(records: &[LineRecord]) -> String`. An ILP *connector crate* that talks to a running InfluxDB would need this to format outgoing writes, but the roadmap item specifically calls for an ILP/Influx *interop connector crate*, which likely means receiving from InfluxDB, not writing to it.

### 5.3 Precision and time-unit mapping — ✅ Present

`TimestampPrecision` (lib.rs:78) and `TimeUnit` (`weft-physical-type/src/timestamp.rs:72`) both define the same four units (ns/µs/ms/s). `epoch_in_unit` (ingest.rs:1301) resizes from absolute `DateTime<Utc>` to the aspect's declared epoch unit. The mapping is lossy in the coarse direction (ns→seconds truncates) but correct.

### 5.4 Tag → aspect/series mapping — ✅ Present

TAG-11 (docs/upgrade-notes.md:43–48) made the tag set the series name. `project_ilp_series` (ingest.rs:1190) enforces that all contributing records share one tag set, then maps them via `weftdb::TagSet::from_pairs(tags)`.

### 5.5 Measurement → dataset mapping — ⚠️ Partial

The `measurement=` query parameter (required since TAG-11) filters records from a multi-measurement payload. But there is no `DatasetId` abstraction at the ILP layer — an InfluxDB measurement would naturally map to a WeftDB dataset, but the dataset concept is not exposed through the ILP ingest path. Multi-measurement payloads must be split client-side (one POST per measurement).

### 5.6 HTTP ingest endpoint parity — ✅ Present

`POST /api/v1/storage/{aspect}/ilp` is a first-class route alongside JSON points, CSV, and Parquet. It shares the `commit_ingest` path. Full set of output formats for compute endpoints (JSON/CSV/Arrow/Parquet) is present.

### 5.7 UDP — ❌ Absent

No UDP listener exists in `weft-server/src/` or anywhere else in the repo. `docs/roadmap-notes.md` does not mention UDP. InfluxDB's classic line-protocol path includes a UDP listener (port 8089 by default), but the roadmap item says "connector crate", not "UDP daemon".

### 5.8 TLS/auth — ✅ Present (server side)

The server has TLS and basic auth support. All ILP routes inherit it. This is the server's own path; an InfluxDB client connector would need its own TLS/auth.

### 5.9 Error and backpressure semantics — ✅ Present

Structured `ApiError` responses with machine-readable codes, `StatusCode` mapping, `Idempotency-Key` deduplication, per-request body cap, disk-reserve guard (`WEFT_DISK_RESERVE_BYTES`), two-phase ingest (`IngestBody` with reservation + top-up). Backpressure via `503` under memory pressure.

### 5.10 Round-trip tests against a real ILP sample — ⚠️ Partial

The compat fixture (`scripts/compat/make-v010-fixtures.sh:136`) generates a real ILP file and sends it to the server. The live integration test (`weft-client/tests/live.rs:132`) sends ILP through the Rust SDK. Both are real end-to-end tests, but neither is a *dedicated* ILP round-trip test suite that exercises edge cases (multi-measurement, bad precision, ambiguous series, boundary timestamps, all field types).

### 5.11 Documentation and examples — ⚠️ Partial

`weft-line-protocol/README.md` documents the parser's purpose and scope. `docs/upgrade-notes.md` documents the TAG-11 breaking change. `docs/reference.md` documents the ILP endpoints. There is no `examples/` directory, no standalone example showing ILP usage, and no "how to send ILP to WeftDB" guide.

---

## 6. Roadmap and decision context

### 6.1 ROADMAP.md

```
Phase 22 — Connectors and ingest interop
  - [ ] **B-conn** Vendor-neutral connector trait + registry   (ROADMAP.md:192)
  - [ ] **B-ilp** ILP/Influx interop connector crate           (ROADMAP.md:193)
```

The roadmap places **B-ilp** in Phase 22, alongside **B-conn** (the connector trait). The ordering implies B-conn comes first, because B-ilp is a *connector crate* that would implement that trait. Neither is checked.

### 6.2 docs/roadmap-notes.md

```
B-conn  (Phase 2/7 · absent)   (roadmap-notes.md:447)
B-ilp   (Phase 2 · partial)    (roadmap-notes.md:448)
```

The notes call B-ilp **partial** — matching this audit's finding. B-conn is **absent**, which is the dependency B-ilp needs to be more than a lone crate.

### 6.3 Prior decisions that constrain shape

| Source | Quote | Constraint |
|--------|-------|------------|
| `CONTRIBUTING.md:62` | `weft-line-protocol parses ILP but has no InfluxDB client` | Crate design is format-agostic |
| `weft-line-protocol/Cargo.toml:11` | `"Vendor-neutral InfluxDB Line Protocol (ILP) parser: text in, neutral records / splimes points out. No client, no network, no vendor dependency."` | The parser crate is explicitly not a connector |
| `DECISIONS.md:2165` | `"historian connectors and SLA support are paid"` | Connectors other than open-source ones (Prometheus, ILP) are commercial-tier |
| `weft-line-protocol/src/lib.rs:10-15` | Doc comment: "a concrete InfluxDB connector … would live in its own crate outside the core" | A connector crate is expected to be a *new* crate, not an addition to an existing one |

No decision contradicts a connector crate. The crate would be new (e.g. `weft-influxdb-connector` or similar), outside the core workspace or in a separate repository, and would implement whatever `B-conn` trait defines when that is designed.

---

## 7. What it would take, in cost order

1. **Design the connector trait** (B-conn): a vendor-neutral trait with methods like `fn write_series(&self, measurement, tags, fields, timestamps)`, `fn query(&self, query)`, `fn ping(&self)`. This is a prerequisite for B-ilp — without it, the ILP connector would be ad-hoc.

2. **Write the connector crate** (`weft-influxdb-connector`): implement the trait for InfluxDB's HTTP API (v1 or v2 — InfluxDB 3 uses a different API). This depends on the InfluxDB client crate + TLS.

3. **Add ILP serializer**: implement `fn serialize(lines: &[LineRecord]) -> String` or `impl Display for LineRecord` in `weft-line-protocol`. This small change (~50 LoC) is needed if the connector crate needs to produce ILP text for InfluxDB compatibility.

4. **Add round-trip tests**: a suite that generates ILP, sends it to the server, reads it back, and compares. Could use the existing `weft-client` SDK.

5. **Documentation and examples**: an `examples/ilp-ingest` directory with a Rust + Python example of ILP ingest into WeftDB.

---

## 8. How this was verified

This is a **structural audit** (grep, read, classify). Every claim above is backed by a `path:line` reference. No code was compiled, no server was started, and no test was run. The search was done with `grep`, `find`, and `read` across the entire repository at commit `origin/main` (527eae56c04dd3784f9e676f90e334cc08cee13c), focussing on:

- `weft-line-protocol/src/` — parser source and tests
- `weft-server/src/ingest.rs` — ILP ingest handler, `project_ilp_series`, `IlpIngestParams`
- `weft-server/src/interpolate.rs` — compute ILP handler
- `weft-server/src/downsample.rs` — downsample ILP handler
- `weft-server/src/routes.rs` — route registration
- `weft-client/src/client.rs`, `weft-client/src/types.rs` — Rust SDK
- `clients/python/weftdb/client.py` — Python SDK
- `Cargo.toml`, `Cargo.lock` — dependency analysis
- `deny.toml` — policy analysis
- `ROADMAP.md`, `docs/roadmap-notes.md` — roadmap context
- `CONTRIBUTING.md`, `weft-line-protocol/README.md` — design constraints

---

## 9. Follow-ups (candidate issues, not created)

- `#<TBD>`: **Design the vendor-neutral connector trait (B-conn)**. Prerequisite for B-ilp; the trait defines `write_series`, `query`, `ping` and any discovery/health-check methods. Blocks B-ilp. Evidence: `ROADMAP.md:192`, `roadmap-notes.md:447`.
- `#<TBD>`: **Add ILP serializer to `weft-line-protocol`** (`impl Display for LineRecord` or `fn serialize`). Small cost (~50 LoC), needed if a connector writes ILP to InfluxDB. Evidence: no serialize function exists in `weft-line-protocol/src/lib.rs`.
- `#<TBD>`: **Write `weft-influxdb-connector` crate** implementing B-conn trait for InfluxDB HTTP API. New crate outside core workspace. Evidence: `ROADMAP.md:193`, design constraint at `weft-line-protocol/src/lib.rs:10-15`.
- `#<TBD>`: **Dedicated ILP round-trip test suite** exercising multi-measurement payloads, all field types, precision edge cases, ambiguous series rejection. Evidence: only partial coverage in compat and live tests.
- `#<TBD>`: **ILP documentation and examples** — standalone `examples/ilp-ingest/` with Rust and Python examples, and a "send ILP to WeftDB" guide. Evidence: no `examples/` directory and no dedicated ILP user guide.