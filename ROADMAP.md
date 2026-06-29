# DSP Roadmap

> **North star.** DSP becomes a commercial product by making **reproducible
> performance evidence** the center of the roadmap. The governing metric is
> **dollars per billion interpolated output points at a specified p95 latency
> target** — it ties DSP's technical differentiator (interpolation-heavy irregular
> time-series) to buyer value.
>
> **Governing rule:** *no major feature advances unless it improves a benchmarked
> customer outcome.* Every performance claim in the README must link to a benchmark
> artifact.

This document merges two inputs: a peer-reviewed commercial/benchmark analysis (the
strategic spine below) and the feature backlog distilled from DSP's **15 predecessor
repositories** (now archived under [`legacy/`](legacy) with full history). The
analysis reprioritizes that backlog; the concrete items live in
[§ Predecessor-derived backlog](#predecessor-derived-backlog), each mapped to a phase.

---

## Table of Contents

- [Commercial thesis & positioning](#commercial-thesis--positioning)
- [Guiding principles & hard constraints](#guiding-principles--hard-constraints)
- [Top-level tracks](#top-level-tracks)
- [Phased roadmap (0–9)](#phased-roadmap-09)
- [Updated priority order](#updated-priority-order)
- [Predecessor-derived backlog](#predecessor-derived-backlog)
- [GPU acceleration detail (Phases 1–4 done, 5+ next)](#gpu-acceleration-detail)
- [Research foundation](#research-foundation)
- [Control-plane engine: Turso/libSQL 0.6 adoption](#control-plane-engine-tursolibsql-06-adoption)
- [Six-month execution plan](#six-month-execution-plan)
- [Benchmark report template](#benchmark-report-template)
- [Business validation](#business-validation)
- [Legal & licensing](#legal--licensing)
- [Immediate next actions](#immediate-next-actions)

---

## Commercial thesis & positioning

**The wrong wedge:** "DSP is a faster general-purpose time-series database." Too hard
to prove early against ClickHouse, InfluxDB 3, QuestDB, TimescaleDB, IoTDB, DuckDB,
and kdb+.

**The wedge to take:**

> **DSP is an interpolation-native, precision-aware, GPU-accelerated time-series
> engine for irregular high-value data, with public benchmarks proving superior
> performance on resampling, gap filling, compression-aware historical analysis, and
> analytical pipelines.**

**Where DSP wins (focus here):** irregular time-series; interpolation-on-read;
upsampling/gap filling; high-precision values; GPU spline execution;
compression-aware historical analysis; event/pattern pipelines that depend on
reconstructed signal shape.

**First commercial claims (each benchmark-backed):**
1. Fastest interpolation-on-read for large irregular ranges.
2. Best cost/performance for gap filling and upsampling.
3. Compression that preserves downstream analytical behavior.
4. Predictable p95/p99 latency under concurrent ingest + query.
5. Transparent benchmarks vs ClickHouse, InfluxDB, QuestDB, TimescaleDB, DuckDB, IoTDB.

**Do NOT claim initially:** "faster than ClickHouse for all analytics"; "better than
InfluxDB for all TSDB workloads"; "GPU makes every query faster"; "arbitrary
precision with no performance cost"; "commercially ready because the TUI works";
"AI-native vector TSDB" before real vector/embedding functionality exists.

Ship honesty pages: *When DSP beats general TSDBs* · *When DSP is not the right tool*
· *Benchmark methodology* · *Interpolation-accuracy methodology* · *GPU tuning &
economics* · *Precision & physical value types*.

---

## Guiding principles & hard constraints

1. **Benchmark-gated features.** A feature is "done" only when `DSP-Bench` shows a
   measured, reproducible, correctness-validated outcome.
2. **Vendor-neutral connectors (hard constraint).** The DSP codebase defines **only**
   a vendor-neutral connector abstraction — ideally a small dedicated crate (e.g.
   `dsp-connector`) holding the `Source`/`Connector` trait + runtime registry. Every
   concrete connector (Thorchain, InfluxDB, CSV, …) lives **outside** the core as its
   own crate that depends on the abstraction, never the reverse. No vendor-specific
   code/types/deps in `splimes`/`database`/`database_orchestration`/`dsp-tui`.
   Thorchain is *one of many* sources and must not be coupled to DSP.
3. **Storage boundary (refined by this analysis).** **libSQL/Turso is the control
   plane** — catalog, metadata, config, pipeline state, transactional control. DSP's
   own **typed columnar measurement segments** (Storage v2) own the high-volume
   measurement hot path. External databases like InfluxDB are *integration targets*
   (source and/or sink) reached through connectors — **never** replacement backends.
   Do not reintroduce `sled` or `sqlx`.
4. **Precision-aware, not precision-taxed.** Keep `BigDecimal` as the logical/API
   type; execute each series in the fastest *safe* physical encoding declared by
   schema. Downcasting is explicit, benchmarked, and governed by schema-level error
   bounds — never a silent `BigDecimal → f64`.
5. **Port designs, not legacy code.** Legacy crates are edition-2021, `.unwrap()`-heavy,
   often nightly microservices. Reimplement against current async + typed `Error`
   conventions, libSQL, and `splimes`.
6. **Honesty / anti-Goodhart.** Publish negative results and workloads where DSP
   loses; methodology before numbers; correctness gates every performance number.

---

## Top-level tracks

The roadmap is reordered around these ten tracks (priority order):

1. **DSP-Bench** — public, reproducible, competitor-facing, customer-runnable.
2. **Benchmark-grade API** — server mode, InfluxDB Line Protocol, REST, Arrow/Parquet, metrics.
3. **Instrumentation** — full timing breakdown ingest → GPU → serialization.
4. **Physical type system** — precision semantics with fast physical encodings.
5. **Storage v2** — libSQL control plane + typed columnar measurement segments.
6. **GPU interpolation** — end-to-end, auto-tuned, portable, economically justified.
7. **Compression v2** — modern codecs, model-based segments, random access, downstream stability.
8. **Operational correctness** — late/out-of-order data, WAL, crash recovery, compaction consistency.
9. **Commercial hardening** — security, compliance readiness, packaging, SDKs, backup/restore.
10. **Analytics premium** — pattern/event/signal pipelines, after the core engine is proven.

---

## Phased roadmap (0–9)

### Phase 0 — Benchmark thesis & buyer wedge · 1–2 wks · *Immediate*

Write a short public performance thesis and the first benchmark claim list (ingest
throughput; raw range-scan latency; interpolation throughput; p95/p99 interpolated
latency; storage bytes/point; compression ratio + accuracy; event-detection
stability; end-to-end GPU speedup; cost per billion interpolated points). Define what
DSP is **not** chasing first (generic OLAP vs ClickHouse; generic monitoring vs
InfluxDB; full SQL/Postgres vs TimescaleDB; finance-tick dominance vs kdb+).

**Acceptance:** you can answer "why choose DSP over InfluxDB/QuestDB/ClickHouse/
TimescaleDB/IoTDB/DuckDB/kdb+?"; 3–5 design partners with interpolation-heavy
workloads identified; willingness-to-pay hypotheses documented.

### Phase 1 — Build `DSP-Bench` · 4–8 wks · *Highest priority*

A `dsp-bench` workspace that is both an internal engineering suite and a
public/customer-runnable diagnostic. Model it on SEER/TSM-Bench (result storage,
dashboards, repeatable config, system adapters, workload profiles) — not ad-hoc
scripts. Implement **TSBS compatibility** (via InfluxDB Line Protocol) for baseline
comparisons, but make DSP-Bench the primary suite.

```text
dsp-bench/
  adapters/      dsp/ clickhouse/ influxdb3/ questdb/ timescaledb/ iotdb/ duckdb/ kdb_optional/
  workloads/     ingest/ online_ingest_query/ range_fetch/ point_lookup/ downsample/
                 upsample_interpolate/ gap_fill/ compression/ compressed_query/ pattern_pipeline/
  datasets/      generated/ tsbs_like/ tsm_bench_like/ scits_like/ finance_ticks/
                 irregular_iot/ scientific_sensor/ missingness/ compression_corpus/
  runners/       local/ docker_compose/ cloud/
  reports/       json/ parquet/ html/ grafana/
  dashboards/  docs/
```

**Systems:** min — DSP, ClickHouse, InfluxDB 3, QuestDB, TimescaleDB, DuckDB; next —
IoTDB, VictoriaMetrics (if monitoring matters), kdb+ only if legally safe.

**Workloads:** bulk ingest (1M→10M→100M→1B growth curves); online ingest+query; raw
range fetch (long/short/sparse/dense/irregular series, high cardinality);
downsampling (min/max/avg/count, OHLC, TWA, percentiles); **upsampling/interpolation
(flagship)** — linear/cubic/polynomial × regular/irregular/sparse/block-missing,
small interactive vs large batch; point lookup/extrapolation; compression; analytics
pipeline (batching, windows, pattern extraction, correlation, signals).

**Phase 1.1 — fair protocol (essential):** ≥10 reps for short tests; report
median/mean/stddev and p50/p95/p99/max with bootstrap CIs; keep raw results. Separate
cold/warm/hot/post-compaction/post-restart. Saturation curves across batch size,
clients, writers, query concurrency, cardinality, dataset size, GPU output size.
Seeded randomized query mixes (publish seeds). Correctness verification (row counts,
result hashes, interpolation tolerance, compression error bounds). Failure tests
(restart during ingest, crash during compaction, network retry, partial/corrupt
segment). Independent reproducibility (versions, SHAs, images, configs, hardware,
drivers, command lines, raw artifacts).

**Phase 1.2 — fair interpolation comparisons:** report three classes where possible —
(A) native in-DB (Timescale gapfill, QuestDB `SAMPLE BY … FILL`, InfluxQL/SQL fill,
ClickHouse ASOF/window, DuckDB window fns, IoTDB fns); (B) portable SQL baseline
(prev/next + window fns + time grid + asof join); (C) client-side (fetch raw →
interpolate in the same Rust/Python client → total end-to-end). Don't hide
unfavorable results.

**Phase 1.3 — anti-Goodharting:** publish negative results and DSP-losing workloads;
methodology before numbers; no hard-coded shortcuts; run customer-supplied workloads;
benchmark code separate from engine code; correctness validation gates every number.
README policy line: *"DSP benchmarks guide real engineering decisions, not synthetic
wins."*

### Phase 2 — Benchmark-grade server/API · 3–6 wks · *Very high*

> **Status (started).** The `dsp-server` crate (axum) now exists with the API
> skeleton (`/health`, `/ready`), the flagship `POST /api/v1/interpolate` over
> the `splimes` engine, the InfluxDB-Line-Protocol ingest endpoint
> `POST /api/v1/interpolate/ilp`, the **downsample/aggregation query**
> (`POST /api/v1/downsample` + `…/ilp` — min/max/avg/sum/first/last over
> epoch-grid-aligned buckets, reductions in `BigDecimal`), the **single-instant
> point query** (`POST /api/v1/interpolate/point` — evaluates the reconstructed
> signal at one instant and labels it interpolated vs extrapolated, the Phase-2
> raw/interpolated/extrapolated distinction), and a Prometheus `/metrics`
> surface. **Stored-range query surface landed (2026-06-28):** the router state
> grew from a bare `SharedMetrics` to a combined `AppState` (metrics +
> `Option<Arc<database::SegmentStore>>`, the latter projected to the legacy
> handlers via `FromRef`), the binary opens a store from
> `DSP_SEGMENT_STORE_ROOT` (else the storage endpoints answer `503`, and
> `GET /ready` reports `segment_store`), and five storage endpoints read the
> on-disk Storage v2 segments: `GET …/storage/{aspect}/range` and
> `…/value-range` stream **Arrow IPC** bytes
> (`application/vnd.apache.arrow.stream`, via the `dsp-arrow-store` bridge so the
> `arrow-*` tree never reaches the lean core), `…/storage/{aspect}/points` is the
> lossless JSON (non-Arrow) range read, and `…/storage/aspects` /
> `…/storage/{aspect}/stats` / `…/storage/stats` expose the declared schemas and
> the materialized **bytes/point** rollups (the north-star cost term over HTTP).
> **HTTP catalog management + ingest landed (2026-06-28, run 2):** the write
> path that populates the store the read endpoints serve — `POST
> …/storage/aspects` **declares** an aspect's schema (physical encoding +
> value tolerance + timestamp unit, the wire tokens inverting the read
> surface's), `POST …/storage/{aspect}/points` **ingests** a JSON batch
> (dense or nullable, single-block or paged via `rows_per_page`), `POST
> …/storage/{aspect}/ilp` ingests an **InfluxDB-Line-Protocol** payload into
> a declared aspect (rescaling each parsed instant to the aspect's declared
> `TimeUnit`, the storage counterpart of the interpolation ILP endpoint), and
> `GET …/storage/catalog` reports the store's `(database, subject)` scope +
> the registered hierarchy. The no-silent-downcast guarantee holds end to end:
> a value unrepresentable under the declared encoding/tolerance is rejected
> `400`, never downcast (hard constraint #4). `GET …/storage/{aspect}/schema`
> reads one aspect's declared schema (the single-aspect counterpart of the
> list/declare), the JSON `…/points` read grew **`offset`/`limit` pagination**
> (backlog B-rest — `total`/`count`/`offset` in the body), and the ingest
> endpoints are instrumented (`dsp_ingest_*_total` Prometheus counters —
> requests/errors/rows/segments). **Parquet interchange + B-rest aliases +
> JSON value-range landed (2026-06-29):** the JSON `…/points` read gained the
> declarative `take` (alias for `limit`) and `page` (1-based) B-rest aliases;
> **Parquet** export/import ships end to end — `GET …/storage/{aspect}/range.parquet`
> and `…/value-range.parquet` serve a stored window as an Apache Parquet file
> (`application/vnd.apache.parquet`, via the `dsp-arrow`/`dsp-arrow-store` bridge,
> so the `parquet`/`arrow-*` tree never reaches the lean core), and `POST
> …/storage/{aspect}/parquet` ingests a Parquet file back into a declared aspect
> (decode + seal in `dsp-arrow-store`, same `dsp_ingest_*` counters + no-silent-
> downcast guarantee as the JSON/ILP ingest); and `GET …/storage/{aspect}/value-points`
> is the JSON counterpart of the Arrow value-range read (lossless decimal-text
> rows in a `[lo, hi]` band, same B-rest pagination). Still to do: OpenTelemetry.

A commercial DB can't lead with an embedded Rust API + TUI. Build an `axum` server:
create DB / subject / aspect; define schema/physical type; batch ingest; range query;
point query; interpolated range query; downsample query; compression trigger; health;
metrics. **Add InfluxDB Line Protocol early** (unlocks TSBS, migration testing,
ecosystem ingest, QuestDB/Influx-style comparisons). Interop priority: REST/JSON →
ILP → Arrow/Parquet import-export → Prometheus metrics → OpenTelemetry → Python SDK →
Rust SDK → Arrow Flight/Flight SQL → SQL surface / DataFusion (later) → Grafana →
Prometheus remote write/read (if monitoring) → R/Arrow workflows.

**Acceptance:** DSP-Bench drives DSP entirely through public APIs; the TUI becomes
secondary (demos/debugging); API responses distinguish raw / interpolated /
extrapolated / compressed / reconstructed values *(this also delivers the
synthetic-point marking nuance — see backlog item B-tags)*.

### Phase 3 — Instrument everything · 2–4 wks · *Very high*

Before optimizing, make bottlenecks visible. Tracing spans for: request parse · auth
· ILP/CSV/Arrow decode · value parse · `BigDecimal` conversion · physical-encoding
conversion · timestamp normalization · WAL append · libSQL write · segment write ·
commit · index update · range read · page skip · cache hit/miss · decompression · CPU
interpolation · GPU upload · GPU queue wait · GPU kernel · GPU readback ·
serialization. Expose `/metrics`, `/debug/profile/current`, `/bench/runs/:id`,
`/health`, `/ready`. **Acceptance:** for any slow benchmark you can say where the time
went (e.g. "52% value parsing, 18% segment read, 15% GPU transfer, 8% kernel, 7%
JSON").

### Phase 4 — Hot path: physical types & Storage v2 · 8–12 wks · *High*

- **4.1 Physical numeric encodings** (per aspect): `F32`, `F64`, `ScaledI64`,
  `ScaledI128`, `Decimal128`, `BigDecimalText` — each declaring storage encoding,
  compression/GPU/interpolation eligibility, exactness guarantees, conversion
  behavior. *(Started — the vendor-neutral **`dsp-physical-type`** crate ships
  all six `PhysicalType` encodings with `encode`/`to_logical` and an explicit
  `Exactness` (Exact / Lossy-with-residual / never-silent) per hard constraint
  #4; a declarative `PhysicalProfile` (storage width, lossless/hot-path
  eligibility); a columnar `encode_column` aggregating exactness + estimating
  bytes/point; and an advisory `recommend_encoding` (fastest-safe selection
  within a tolerance). `BigDecimal` stays the logical type. **Bench wiring
  landed:** DSP-Bench's `BenchResult` now carries a `StorageEstimate` block
  (schema v6) — `recommend_encoding` over the stored value column plus the
  timestamp column (4.2 below) gives a measured **total bytes/point**
  (`StorageEstimate::from_columns`), the north-star cost term, surfaced in the
  HTML report (`enc` / `val B/pt` / `tot B/pt`). **Schema-level declaration
  landed:** `dsp-physical-type::schema::AspectSchema` declares, per aspect, the
  `PhysicalType`, its permitted per-value error bound, and the timestamp
  `TimeUnit`; `AspectSchema::seal` builds a segment under the *declared*
  encoding (vs `Segment::build`'s advisory `recommend_encoding`) and **errors**
  rather than silently downcasting — `SealError::Encode` for an unrepresentable
  value, `SealError::ToleranceExceeded` when the declared encoding would lose
  more precision than the schema permits (hard constraint #4, enforced). Still
  to do here: the Storage v2 segment store wiring that consumes the declaration.)*
- **4.2 Timestamp semantics:** integer epoch internally (ns/µs as needed), explicit
  tz + leap-second policy, monotonic ordering; delta / delta-of-delta / bit-pack / RLE.
  *(Started — `dsp-physical-type::timestamp` ships lossless **delta** and
  **delta-of-delta** transforms over `i64` epochs (`TimeUnit` = seconds/millis/
  micros/nanos), wrapping-safe round trips, a zig-zag + LEB128 varint byte
  estimate, and **RLE** of the second-difference stream with a `best_estimated_bytes`
  selector (varint vs RLE, whichever is smaller). A regular 1000-point column
  packs to ~12 bytes total. The bench timestamp bytes/point uses this. Still to
  do: bit-packing, explicit tz + leap-second policy, monotonic-order enforcement.)*
- **4.3 Columnar segment store** (`AspectStorageMode::{LibSqlRows, SegmentedColumnar,
  Hybrid}`): append-friendly, immutable-after-seal, compactable, checksummed,
  page-indexed, random-access. Layout: `catalog.db` + per-aspect `metadata.db`,
  `segments/*.dspseg`, `segment_index.db`. Per-segment: timestamp/value/quality
  columns, physical type, codec, min/max ts + value, row/null counts, page offsets,
  per-page stats, checksums, version. Arrow-compatible memory internally; Parquet
  import/export from day one — a custom `.dspseg` is justified only for
  interpolation/random-access performance.
  *(Started — `dsp-physical-type::segment` ships the in-memory shape of a
  `.dspseg`: a `Segment` binding the Phase-4.1 typed value column
  (`ColumnEncoding`) and the Phase-4.2 delta-of-delta/RLE timestamp column
  (`DeltaOfDeltaColumn`) with per-segment min/max ts, min/max value, row count,
  and a format `version`, plus an exact `decode` round trip. Its
  `bytes_per_point` shares `dsp-physical-type`'s column estimators with the
  DSP-Bench `StorageEstimate` (single codec source of truth via
  `best_encoding_name`), so the advisory bench estimate and a realized segment
  cannot drift. **On-disk `.dspseg` frame landed:** the
  `dsp-physical-type::dspseg` module ships the hand-rolled, versioned,
  checksummed binary layout a `Segment` seals to — byte primitives (checked
  little-endian ints, LEB128 / zig-zag varints, length-prefixed UTF-8) + an
  IEEE CRC-32, a value-column codec (IEEE byte patterns / varint mantissa /
  `i128` / length-prefixed text per `PhysicalType`), a timestamp-column codec
  (delta-of-delta + varints), and the framed segment (`write_segment` /
  `read_segment`, `Segment::write_to` / `read_from`): a `DSPSEG\0` magic, the
  format version, the per-segment stats header (the data-skipping inputs),
  the two column blocks, and a trailing CRC verified **before** parse so a
  corrupt/truncated frame fails fast rather than being misread. Decidedly
  *not* a `bincode`/serde blob (which cannot round-trip `BigDecimal`).
  **Quality/null column landed:** the `dsp-physical-type::nulls` module ships a
  `NullMask` (per-row presence bitmap, LSB-first, `ceil(n/8)` bytes — a fully
  dense column stores zero mask bytes, so a non-nullable segment's bytes/point
  is unchanged); `Segment::build_nullable` takes a `&[Option<BigDecimal>]` value
  column, stores only the present values densely, and `decode_nullable`
  reconstructs the gaps as `None` — making `null_count` real. The `.dspseg`
  frame bumped to **format version 2**: a quality-column block (presence flag +
  length-prefixed bitmap) after the timestamp column, validated against the
  header row/null counts on read (`DspSegError::InvalidNullMask`) after the CRC
  gate. `AspectSchema::seal_nullable` declares + enforces the encoding over the
  present values (remapping an `Encode` error's index back to the original row,
  nulls included). **Intra-frame page subdivision landed:** the
  `dsp-physical-type::page` module ships a `Page` (one fixed-height block of
  rows — a value column, a delta-of-delta timestamp column, a `NullMask`, and
  its **own** min/max ts/value stats) and a `PagedSegment` that partitions an
  aspect's rows into pages of `rows_per_page` (last page may be shorter), each
  independently encoded, with a segment-level stats rollup. Its on-disk frame is
  a **separate format version 3** (`write_paged_segment` / `read_paged_segment`,
  `PagedSegment::write_to` / `read_from`, distinct from the single-block v2 — the
  two readers reject each other's frames as `UnsupportedVersion`): magic +
  version + `rows_per_page` + segment-level stats + a **per-page index table**
  (each page's full stats *and* its column-block byte length) + the concatenated
  pure-column page blocks + a trailing CRC verified before parse. The per-page
  index means a reader prunes pages on their min/max ts/value **without touching
  a column byte**, and the block lengths let it **seek** straight to a wanted
  page's bytes; each page block is bounded to its indexed length on read (a page
  that doesn't fill its block is rejected as `TrailingBytes`). **Schema-declared
  paged seal landed:** `AspectSchema::seal_paged` / `seal_paged_nullable` build a
  `PagedSegment` under the *declared* `PhysicalType` (the hard-constraint-#4
  counterpart of `PagedSegment::build`'s advisory `recommend_encoding`),
  enforcing the tolerance bound **per page** and remapping an `Encode` error's
  index to the global row (past page nulls and prior pages) — so paged storage
  carries the same no-silent-downcast guarantee single-block `seal` does.
  **Segment-index control plane + on-disk store landed:** the vendor-neutral
  `dsp-physical-type::catalog` ships a `SegmentDescriptor` (one index row per
  sealed segment — min/max ts/value, row/null counts, byte length, path,
  encoding) derived directly from a sealed `Segment`/`PagedSegment`, and a
  resident `SegmentIndex` that prunes a query to the segments it must open
  (`prune_by_time`/`prune_by_value`/`prune_present_by_time`) without reading a
  `.dspseg` byte. `database::SegmentIndexStore` persists those descriptors in the
  libSQL control plane (hard constraint #3 — metadata only, never measurements)
  and answers a time-range query with a SQL `WHERE` over the indexed integer
  min/max-ts columns (the `BigDecimal` bounds round-trip as plain text and the
  `PhysicalType`/`TimeUnit` as JSON, no silent downcast even in the catalog).
  `database::SegmentStore` closes the loop against the filesystem: it seals a
  batch under an `AspectSchema` to `segments/<aspect>-<id>.dspseg` (single-block
  *and* paged frames), records the descriptor, and reads back by pruning the
  index first then opening **only** the surviving files — `read_time_range`
  (frame-version aware, paged frames skip pages within the file too),
  `read_value_range` (resident-index value pruning), and an `aspect_stats`
  surfacing the realized north-star bytes/point. **catalog.db hierarchy +
  schema-aware store landed:** `database::CatalogStore` registers the upper two
  levels of the `catalog.db` hierarchy — `databases` (one row per name) and
  `subjects` (`(database, subject)`, refusing a subject whose database is not
  registered, cascading on `remove_database`); `database::AspectCatalog`
  (already shipped) carries the per-aspect `AspectSchema`, now with a
  `list_all` flat enumeration of every declared `(database, subject, aspect)`
  for introspection/recovery. `SegmentStore` is now **schema-aware**: it owns an
  `AspectCatalog` + `CatalogStore` and a `(database, subject)` scope
  (`open_scoped`), registers itself in `catalog.db` on open, and seals by
  lookup — `declare(aspect, schema)` once, then
  `seal_declared`/`seal_declared_nullable`/`seal_declared_paged` without
  re-supplying the schema (an undeclared aspect is refused, not guessed); a
  reopened store recovers the encoding it sealed under. **Per-aspect
  `metadata.db` landed** (the last unbuilt layer of the `catalog.db` +
  `metadata.db` + `segments/` + `segment_index.db` layout):
  `database::AspectMetadataStore` materializes one segment-set rollup row per
  aspect (`AspectMetadata` — segment/row/null counts, total framed bytes, and
  the aspect-wide min/max ts *and* value spans), so the aspect-wide summary
  `SegmentStore::aspect_stats` computes by scanning the whole resident index is
  now an O(1) `metadata.db` read (`SegmentStore::aspect_metadata`). The rollup is
  folded forward one descriptor per seal (`record_seal`) but always
  re-derivable from the durable segment index (`AspectMetadata::from_index`):
  `SegmentStore::rebuild_aspect_metadata` / `rebuild_all_metadata` reconcile a
  diverged or lost rollup against the index (the source of truth), enumerated via
  the new `SegmentIndexStore::list_aspects`. `SegmentStore::store_stats` sums the
  rollups into the subject-wide north-star bytes/point (`StoreStorageStats`),
  control-plane only and without opening a segment. Still to do here:
  Arrow/Parquet interchange.)*
- **4.4 Data skipping** (time/value/tag/quality pruning, page skipping).
  *(Started — segment-level pruning on `dsp-physical-type::Segment`:
  `overlaps_time`/`contains_timestamp` and `may_contain_value` (conservative —
  `false` only when safe to skip), plus `prune_by_time`/`prune_by_value`
  selecting exactly the overlapping segments out of a set. **Quality pruning
  landed:** with the `NullMask` quality column now real, `Segment::is_all_null`
  / `present_count` / `present_count_in_range` and the free `prune_present_by_time`
  skip segments that overlap a window but hold only nulls there — strictly more
  selective than `prune_by_time` (a value-bearing query gets nothing from an
  all-null segment). **Intra-segment page skipping landed:** on the Phase-4.3
  `PagedSegment`, `prune_pages_by_time` selects the pages overlapping a window
  and `read_time_range` decodes **only** those pages (the columns of skipped
  pages are never reconstructed); `prune_present_pages_by_time` /
  `present_count_in_range` add the quality-aware page-level mirror (skip all-null
  pages too). Still to do: **tag** pruning (per-measurement tags/labels — backlog
  B-tags — do not exist yet, so tag-based skipping has nothing to prune on).)*
- **4.5 Arrow-compatible arrays** (eases Python/Flight/DataFusion/Parquet).
  *(Started — the vendor-neutral **`dsp-arrow`** crate ships the Apache Arrow
  interchange for a sealed `dsp-physical-type` `Segment`/`PagedSegment`, kept in
  its own leaf crate (depends on `dsp-physical-type`, never the reverse) so the
  heavy `arrow-*` dependency never reaches the hot-path core. Apache Arrow is an
  open in-memory interchange standard — not a storage backend (hard constraint #3
  untouched: `.dspseg` still owns the hot path) and not a vendor connector (hard
  constraint #2 untouched). `segment_to_record_batch` emits a lossless two-column
  batch — `timestamp: Int64` + `value: Utf8` (plain decimal text, Arrow validity
  for nulls) — with the segment's `TimeUnit`/physical-encoding/version in the
  self-describing schema metadata; `record_batch_to_columns` /
  `segment_from_record_batch` invert it. A **typed numeric fast path**
  (`segment_to_record_batch_typed`) emits the natural Arrow array per physical
  encoding: `F64 -> Float64`, `F32 -> Float32`, and the fixed-scale
  `ScaledI64`/`ScaledI128 -> Decimal128` **exactly** (no float rounding), with the
  per-value-scale `Decimal128` and variable-width `BigDecimalText` falling back to
  the always-correct text column. The reader dispatches on the value column's
  actual Arrow type, so either form round-trips, honoring hard constraint #4 (no
  silent downcast — text and Decimal128 paths are exact, the float path reproduces
  the already-stored f64/f32 bits). **`PagedSegment` interchange** ships too:
  `paged_segment_to_record_batches` emits one batch per page (the Arrow-idiomatic
  stream, preserving the Phase-4.4 page-skipping structure),
  `paged_segment_to_record_batch` collapses to one, and
  `record_batches_to_columns` / `paged_segment_from_record_batches` invert them.
  **Logical-column + stored-range interchange landed:**
  `dsp_arrow::columns_to_record_batch` / `columns_to_record_batch_typed` build a
  batch directly from logical `(Vec<i64>, Vec<Option<BigDecimal>>)` columns (the
  shape a stored read returns, possibly spanning several segments) — the typed
  form emits the natural `Float64`/`Float32`/exact-`Decimal128` column for the
  aspect's single declared encoding, text otherwise. A new leaf bridge crate
  **`dsp-arrow-store`** (depends on *both* `database` and `dsp-arrow`, so the
  heavy `arrow-*` tree never reaches the core — `database`/`splimes`/
  `dsp-physical-type` stay arrow-free) reads a stored aspect range straight into
  a `RecordBatch`: `read_time_range_to_record_batch[_typed]` and
  `read_value_range_to_record_batch` (recovering the declared `TimeUnit`/encoding
  from the aspect schema). And the **Arrow IPC wire format** ships:
  `dsp_arrow::write_ipc_stream` / `read_ipc_stream` serialize a batch set to/from
  the self-describing IPC stream bytes, and `dsp-arrow-store`'s
  `read_time_range_to_ipc_bytes` / `read_value_range_to_ipc_bytes` take a stored
  read all the way to portable bytes ready for an HTTP body / Arrow Flight / a
  `.arrow` file. 31 `dsp-arrow` + 9 `dsp-arrow-store` tests; 0 clippy warnings
  under pedantic+nursery. **The Phase-2 `dsp-server` HTTP endpoint exposing the
  IPC-bytes export landed (2026-06-28)** — `GET /api/v1/storage/{aspect}/range`
  and `…/value-range` serve `read_time_range_to_ipc_bytes` /
  `read_value_range_to_ipc_bytes` as `application/vnd.apache.arrow.stream` once a
  `SegmentStore` is configured (see the Phase-2 status note). **Parquet
  import/export landed (2026-06-29):** `dsp_arrow::write_parquet` / `read_parquet`
  serialize a `RecordBatch` set to/from an Apache Parquet file (embedding the
  self-describing Arrow schema so DSP's time-unit/encoding metadata survives), the
  `parquet` dep pulled with `default-features = false, features = ["arrow"]` so no
  compression-codec C libraries reach the build (only 2 new crates resolve);
  `dsp-arrow-store` takes a stored read all the way to Parquet bytes
  (`read_time_range_to_parquet_bytes` / `read_value_range_to_parquet_bytes`) and
  ingests a Parquet file back into a declared aspect
  (`ingest_parquet_into_aspect`, sealing under the declared encoding); and the
  `dsp-server` `…/range.parquet` / `…/value-range.parquet` / `POST …/parquet`
  endpoints expose all three over HTTP. The typed exact-`Decimal128` and text
  paths stay byte-faithful (hard constraint #4). Phase-4.5 Arrow/Parquet
  interchange is now complete.)*
- **4.6 Correctness semantics:** out-of-order/late data, dedup, upsert, idempotent
  batch ingest, clock skew, precision, tz parsing, leap seconds, query consistency
  during compaction, read-your-writes, snapshot isolation.

**Acceptance:** range scans avoid materializing `BigDecimal` unless requested;
interpolation streams from typed arrays; page/segment pruning visible in debug output;
DSP-Bench shows ingest/scan/compression gains; correctness tests cover late + OOO data.

### Phase 5 — GPU interpolation flagship · 6–10 wks (parallel w/ Phase 4) · *High*

> **Note — interpolation/extrapolation already works.** `Outputs::analyze_range` /
> `analyze_point` already interpolate **and** extrapolate (linear/quadratic/cubic/
> polynomial on CPU, SIMD, and GPU; tested in `db_tests.rs`). Phase 5 is about making
> the GPU path *fast end-to-end and economically justified*, not adding the capability.

Order: **5.1** command batching → **5.2** true async GPU handles (non-blocking,
overlap CPU parse/read with GPU) → **5.3** CPU/GPU overlap (chunked read → decode →
upload → kernel → stream output) → **5.4** hardware auto-tuning (calibrate CPU/GPU
throughput, upload/readback cost, break-even sizes; auto-select) → **5.5** GPU
economics (cloud cost, CPU-only fallback, utilization threshold, contention, cold
start, NVIDIA/AMD/Intel/Apple portability) → **5.6** wgpu/WGSL portability &
determinism (conformance matrix, numerical drift, published tolerances) → **5.7**
defer multi-GPU until single-GPU wins are proven. GPU benchmarks must be **end-to-end**
(storage read → decode → filter → Decimal convert → transfer → queue wait → kernel →
readback → serialize); a kernel-only speedup is not commercially credible.

**Acceptance:** publishable claim like *"on hardware X, DSP produces Y M interpolated
points/sec for irregular cubic interpolation including storage read, decode, GPU
transfer, kernel, readback, and API serialization, p95 = Z."* (See
[GPU detail](#gpu-acceleration-detail) for the shipped Phases 1–4 + the legacy
5/5.5/6/7 plan now folded here.)

### Phase 6 — Compression v2 · 8–12 wks · *High*

- **6.1 Lossless typed codecs:** timestamp delta/delta-of-delta; scaled-int bit
  packing; RLE for regular intervals; Gorilla/Chimp-style f64; ALP-inspired
  vectorized f64; Decimal128/scaled-int codecs; **block-level random access**.
  *(Check codec patents/licenses before embedding.)*
- **6.2 Model-based compression** (leverages DSP's spline DNA, NeaTS-like): piecewise
  linear / spline / polynomial / nonlinear approximation with bounded residuals;
  lossless-residual option; lossy with max-error guarantee; extrema-preserving mode.
- **6.3 Late/compressed-domain execution:** min/max/count from metadata; predicate
  eval before decompression; interpolate directly from model segments; event
  detection over compressed summaries; CPU filter before GPU transfer. (More practical
  near-term than GPU-initiated storage IO.)
- **6.4 Quality benchmarks:** bytes/point, compress/decompress throughput, random-
  access + range latency, interpolation-after-compression, RMSE/MAE/max-error/bias,
  extrema preservation, event-detection stability, forecasting impact.

**Acceptance:** honest claim like *"this mode cuts storage by X while preserving event
detection within Y% and improving historical query latency by Z."*

### Phase 7 — Online perf, durability, correctness · 6–10 wks · *High for beta*

- **7.1 Online ingest:** scheduled polling daemon, runtime source registration, retry
  buffer, backpressure, idempotency ledger, at-least-once, dedup/upsert, late-arrival
  policy. *(Maps backlog items B-poll, B-retry, B-register, B-dedup.)*
- **7.2 WAL & crash consistency:** WAL design, segment-seal protocol, atomic catalog
  updates, recovery, partial-write handling, fsync policy, durability modes.
- **7.3 Corruption detection:** segment/page checksums, catalog checks, startup
  verification, repair tooling.
- **7.4 Backup/restore:** online backup, PITR if feasible, verification, drills,
  documented RPO/RTO.
- **7.5 Compaction:** scheduling, query consistency during compaction, resource
  limits, metrics, cancellation, priority.
- **7.6 Quotas/limits:** tenant/disk/memory/request-size/query-timeout/GPU-memory.

**Acceptance:** a 24-hour run sustains continuous ingest + concurrent interpolation +
range scans + compression + compaction + late arrivals + simulated failures + restart/
recovery, with bounded p99 and no loss beyond the declared durability mode.

### Phase 8 — Commercial hardening · 8–12 wks · *Required for paid beta*

Security (TLS, API keys, token auth, basic RBAC, service accounts, secrets,
encryption at rest, audit logs, vuln process); compliance readiness (SOC 2, HIPAA
where targeted, GDPR delete/export, retention, tenant isolation); packaging (static
binaries, Docker, Compose, Helm later, systemd, config schema, migration/upgrade/
rollback); observability (Prometheus, Grafana, OTel, structured logs, bench dashboard,
query profiles); SDKs (Rust → Python → TypeScript → R/Arrow); licensing/legal
(open-core vs commercial, comparative-benchmark terms, kdb+ restrictions, dependency +
codec licenses, customer-data handling, trademark use).

**Acceptance:** a design partner can deploy DSP, ingest, run DSP-Bench, inspect
metrics, recover from a restart, and file useful support tickets.

### Phase 9 — Analytics premium · after benchmark foundation · *Medium*

Keep the pattern/event/signal roadmap but don't let it block the benchmark-first
engine. Order: sliding windows → overlapping windows → event-centered windows →
resample-before-compare → pattern dedup → occurrence-distance constraints →
correlation/signal benchmarking → per-stage audit trail. *(Maps backlog Themes 5, 6,
8.)* Future ML/AI (self-supervised event prediction, causal anomaly detection,
temporal point processes, forecasting export, embedding search over shape summaries)
are **future integrations, not the first identity** — do not prematurely rebrand as a
vector DB.

**Commercial tiers:** *DSP Core* (storage/query/interpolation/compression/benchmarks)
· *DSP Accelerated* (GPU) · *DSP Analytics* (pattern/event/signal) · *DSP Enterprise*
(security/compliance/support/deployment/benchmark consulting).

---

## Updated priority order

**Move up immediately:** 1) DSP-Bench · 2) public API/server · 3) InfluxDB Line
Protocol ingest · 4) Prometheus/OpenTelemetry instrumentation · 5) physical numeric
encodings · 6) Storage v2 columnar segments · 7) GPU batching + async · 8)
compression v2 prototypes · 9) online ingest/query benchmarks · 10) correctness/
durability semantics.

**Keep, but narrow:** connector abstraction (start with ILP, CSV, Parquet, HTTP) ·
TUI (demos/debugging, not the primary interface) · pattern/event pipeline (after
engine benchmark credibility) · GPU memory-mapped buffers (after transfer bottlenecks
are measured).

**Defer:** Thorchain-specific connector · multi-GPU · full distributed Raft/cluster ·
GPUDirect/BaM direct storage · full vector-DB positioning · large TUI expansion ·
vendor-specific integrations before benchmark proof · deep ML/event prediction before
core engine wins.

---

## Predecessor-derived backlog

The concrete items mined from the 15 archived repos, **re-mapped to the phases above**.
Status: 🔴 absent · 🟡 partial/verify · 🟢 exists, enhance · ✅ done.

| ID | Item | Status | Phase | Source |
|----|------|--------|-------|--------|
| B-tags | **Per-measurement tags/labels** (incl. `interpolated=true`/provenance) — `measurement.rs` has none. | 🔴 | 2/4 | `DSM-Database`, `DSM-Measurement` |
| B-conn | **Vendor-neutral connector trait + registry** (core). | 🔴 | 2/7 | `dsm-source`, `dsm-asset` |
| B-ilp | **InfluxDB Line Protocol ingest** (and ILP/Influx **interop** connector, separate crate — *not* a storage swap). | 🟡 | 2 | `dsm-influxdb`, `dsm-batch` |
| B-rest | **REST facade** + **declarative query-params** (`range`/`take`/`count`/`page`/`interpolation`) + **pagination**. *(🟡 partial: `dsp-server` serves range (`start`/`end`) + **pagination** (`offset`/`limit`/`take`/`page` + `total`/`count`) on `…/points` **and** `…/value-points` (JSON value-range read); `interpolation` alias + cursor paging still to do.)* | 🟡 | 2 | `DSM-Database` |
| B-poll | **Scheduled polling daemon** (per-source interval) + **B-retry** at-least-once retry buffer + **B-register** runtime source registration. | 🔴 | 7 | `DSM-Input-Module` |
| B-interp | **Interpolate-on-read, single-instant lookup, out-of-range extrapolation.** | ✅ | 5 | `splimes` / `database` (already implemented) |
| B-analysis | **Per-point analysis model** (signed neighbor distance, slope-segmented trends, max-normalized relative vectors) — verify `Trend`/`Relative`/`MeasurementVector`/`Analysis` are wired end-to-end. | 🟡 | 4/9 | `dataset_management`, `DSM-Measurement` |
| B-object | **Schemaless object/annotation store** beside numeric aspects. | 🔴 | 4 | `DSM-Database` |
| B-windows | **Sliding/overlapping + event-centered windows**; **multi-resolution horizon fan-out**; **min-density validation**; **config-driven batching policy**. | 🔴/🟡 | 9 | `dsm-batch`, `DSM-Batcher` |
| B-dedup | **Event-UUID idempotency ledger** + delete-after-success queue. | 🟡 | 7 | `dsm-batch`, `DSM-Batcher` |
| B-sim | **Full variability-metric matrix** (`static`/`absolute_static`/`percentage`/`absolute_percentage` × `Max`/`Avg`/`Sum`); **fixed-point dictionary dedup**; **occurrence-distance constraint**; **resample-before-compare** (wire to `splimes`). | 🟡 | 9 | `DSM-Pattern` |
| B-precision | **`BigDecimal` math audit** (no float drift in pattern coords); **improved `simplify`** (local-extrema). | 🟡 | 4/6 | `DSM-Batch-v2` |
| B-resilience | **Write retry (backoff+jitter)** + **size-tiered write selector**; **interpolation window extension** (±N steps). | 🟡 | 4/7 | `legacy/database` |
| B-msgpack | **MessagePack** for queued artifacts (vs JSON). | 🔴 | 6 | `dsm-batch` |
| B-snapshot | **Pipeline state-snapshot audit trail** (before/after JSON per stage). | 🔴 | 3/9 | `DSM-Log`, `DSM-Batch-v2` |

### Archived source map

| Legacy crate | Mine it for |
|--------------|-------------|
| [`legacy/dsm-source`](legacy/dsm-source) · [`legacy/dsm-asset`](legacy/dsm-asset) | Connector trait / `ToAsset` self-describing-target pattern |
| [`legacy/dsm-influxdb`](legacy/dsm-influxdb) | InfluxDB 2.x client, line protocol, Flux |
| [`legacy/DSM-Thorchain`](legacy/DSM-Thorchain) | Midgard price client, windowed backfill (external connector only) |
| [`legacy/DSM-Input-Module`](legacy/DSM-Input-Module) | Per-source scheduling, retry buffer, runtime registration |
| [`legacy/DSM-Database`](legacy/DSM-Database) | REST surface, tags, object buckets, query params, pagination |
| [`legacy/DSM-Measurement`](legacy/DSM-Measurement) | Enriched measurement model (raw-vs-processed, per-point analytics) |
| [`legacy/database`](legacy/database) | Retry+backoff writes, size-tiered batching, window extension, typed errors, TTL/LRU cache |
| [`legacy/dsm-batch`](legacy/dsm-batch) · [`legacy/DSM-Batch-v2`](legacy/DSM-Batch-v2) · [`legacy/DSM-Batcher`](legacy/DSM-Batcher) | Windows, dedup, horizon→interp mapping, MessagePack, BigDecimal math, multi-horizon fan-out |
| [`legacy/DSM-Pattern`](legacy/DSM-Pattern) · [`legacy/DSM-Patterner`](legacy/DSM-Patterner) | Variability metrics, fixed-point dedup, occurrence-distance, pipeline semantics |
| [`legacy/dataset_management`](legacy/dataset_management) · [`legacy/DSM-Log`](legacy/DSM-Log) | Per-point `Analysis` model intent; state-snapshot logging |

---

## GPU acceleration detail

*DSP's own `splimes` GPU track. Phases 1–4 shipped; 5+ fold into commercial Phase 5.*

**Phases 1–4 — completed (what shipped, in `splimes/src/gpu/`):**

| Phase | Module | What it does |
|-------|--------|--------------|
| 1 — Buffer pool | `buffer_pool.rs` | Size-tiered (4 KB–128 MB), LRU-evicted; cut allocations 7,000+ → <50 per 1M-point run. |
| 2 — Staging buffers | `staging_buffer_manager.rs` | 3-buffer round-robin, persistent mapping; removes unmap/remap cycles. |
| 3 — Async handle | `async_handle.rs` | `GpuInterpolationResult<T>` + `IntoFuture` (computes synchronously today; true async = 5.5). |
| 4 — Configuration | `config.rs` | `GpuConfig` presets + `prewarm_gpu_with_config()` / `gpu_buffer_pool_stats()`. |

GPU presets: `minimal()` 64 MB/1/4 · `low_memory()` 128 MB/2/8 · `default()` 512 MB/3/16
· `high_performance()` 1 GB/4/32 (pool / staging buffers / command batch).

**Phases 5–7 (now under commercial Phase 5):** 5 command batching (est. +50 ms) →
5.5 true async / deferred-readback handle → 6 memory-mapped I/O (est. +30 ms; *only
after transfer bottlenecks are measured*) → 7 multi-GPU (*deferred*). Cumulative
prediction (streamed): Phases 1–4 ≈ 600→700 ms (~80% off the 600–1400 ms baseline);
full set targets **200–400 ms/batch**. Profile with `cargo flamegraph`, the `wgpu`
validation layer, and custom timing.

---

## Research foundation

**Benchmarking:** static microbenchmarks aren't enough. *TSM-Bench* (PVLDB'23) and
*SEER* (PVLDB'24) → macrobenchmarks with realistic data, concurrency, dashboards,
reproducibility; DSP-Bench should resemble SEER. *SciTS* → test growth curves
(10M→100M→1B). *TSBS* → implement for compatibility (via ILP) but not as the primary
proof.

**Competitor architecture (don't benchmark stale mental models):** *InfluxDB 3*
(FDAP: Flight/DataFusion/Arrow/Parquet) — compete where it's less specialized
(interpolation-heavy irregular, precision, GPU resampling, model compression), not on
broad scans. *TimescaleDB Hypercore* (hybrid row/columnar) → DSP needs its own
hot/cold story. *ClickHouse* (vectorized OLAP) → don't fight on generic OLAP first.
*QuestDB* (ILP, `SAMPLE BY`, ASOF) → strong finance/high-ingest comparison. *Apache
IoTDB/TsFile* → validates a time-series-native columnar file layout (typed columns,
page stats, compressed blocks, random-access indexes). *DuckDB* → excellent CPU-only
baseline. *kdb+* → optional; **legal review before publishing comparisons.**

**Storage:** columnar timestamp/value layout, typed encodings, page/chunk metadata,
min/max stats, random-access indexes, per-block compression, data skipping → Storage v2.

**Compression:** *Chimp* (PVLDB'22), *Elf* (PVLDB'23), *ALP* (SIGMOD'24, vectorized),
*NeaTS* (ICDE'25, nonlinear + random access — aligns with DSP's spline DNA). Judge
compression by *downstream task impact* (forecasting/interpolation error, event
stability, extrema preservation), not only bytes.

**GPU databases:** the bottleneck is end-to-end data movement (PCIe, capacity,
orchestration), not kernel speed. *Vortex* (PVLDB'24/25) out-of-core GPU; *Scaling
GPU DBs Beyond GPU Memory* (PVLDB'25) hybrid CPU-filter/GPU-compute. *GPUDirect/BaM*
= long-term research, CUDA-centric, not `wgpu`-portable → defer.

**Irregular/missing data:** *TSI-Bench* etc. → missingness pattern matters; MCAR can
mislead; block/subsequence missingness is harder; linear interpolation is a strong
baseline → benchmark interpolation **quality** (extrema preservation, event-detection
stability) as well as speed.

**ML/event research:** promising future premium layer (self-supervised prediction,
causal detection, temporal point processes) — but storage + interpolation +
compression + benchmark proof first.

---

## Control-plane engine: Turso/libSQL 0.6 adoption

DSP pins `turso = "0.6"` (bumped from 0.4). Per hard-constraint #3, Turso/libSQL is
the **control plane** (catalog, metadata, config, pipeline state, transactions) — it
does **not** own the measurement hot path (that is Storage v2's typed columnar
segments). The features below are evaluated **only** for control-plane use; none of
them turn Turso into the measurement backend.

**Migration already applied (0.4 → 0.6):** Turso 0.6 rejects `AUTOINCREMENT` under
`PRAGMA journal_mode=experimental_mvcc` at parse time (it was tolerated in 0.4). All
control-plane schemas were migrated from `INTEGER PRIMARY KEY AUTOINCREMENT` to plain
`INTEGER PRIMARY KEY` (still a rowid alias that auto-assigns on insert; DSP never
relied on the monotonic-no-reuse guarantee). MVCC concurrent writes (`BEGIN
CONCURRENT`) remain the basis of the write path.

**Adopt (mapped to phase):**

| Turso 0.6 feature | DSP use (control plane) | Phase |
|-------------------|-------------------------|-------|
| **Production MVCC concurrent writes** (`BEGIN CONCURRENT`, no "database is locked") | Already the write path; now non-experimental — lean on it for concurrent ingest + catalog updates under load. | 7 |
| **Encryption at rest** (AEAD pager + chunked 32 KiB frame encryption; the MVCC `.db-log` is also encrypted) | Encrypt catalog/metadata/pipeline-state DBs that hold tenant config + provenance. | 8 |
| **`VACUUM INTO 'file'`** (compacted copy) + in-place `VACUUM` | Online, consistent **backup/snapshot** of control-plane DBs without a custom dumper; feeds backup/restore + RPO/RTO drills. | 7.4 |
| **Triggers (now production)** — `BEFORE/AFTER/INSTEAD OF` + `WHEN` | Enforce catalog invariants and emit **audit-log** rows on metadata mutations. | 7/8 |
| **`Statement::n_change()`** (affected-row counts) | Exact write accounting for idempotent batch ingest + per-stage instrumentation spans. | 3/7 |
| **Dynamic auth tokens as closures** (credential rotation) | Hosted/remote control-plane auth without restart. | 8 |
| **`UPDATE … FROM`, aggregate `FILTER`, `INDEXED BY`, `NULLS FIRST/LAST`** | Simplifies catalog/metadata queries; lets the planner be steered explicitly. | 2/4 |

**Evaluate, do not rush:**

- **Native vector search / embedding storage + cosine similarity.** Real, in-engine
  now — but the roadmap is explicit: *do not rebrand as a vector DB before real
  embedding functionality exists*. Park this for **Phase 9** shape-summary /
  pattern-embedding search, as a control-plane index over *summaries*, never over raw
  measurements.
- **Change Data Capture (CDC) / sync engine.** Potentially useful for online
  ingest/replication (Phase 7) and for shipping catalog changes to a hosted control
  plane — assess once single-node durability (7.2) is solid.
- **Custom I/O (`with_io_impl`)** and **generated columns / domains / array types.**
  Advanced; only if a concrete control-plane need appears. No measurement-path use.

**Explicitly defer / avoid:**

- **Multi-process WAL access** (`?experimental=multiprocess_wal`) — lets external tools
  read a live `.db`, but it is **incompatible with `BEGIN CONCURRENT`**, which DSP's
  MVCC write path depends on. Not worth losing concurrent writes; revisit only if a
  hard live-inspection requirement emerges.
- Treating Turso vector search or any Turso table as the **measurement store** — this
  violates hard-constraint #3.

---

## Six-month execution plan

- **Month 1 — Benchmark truth:** `dsp-bench` workspace; DSP + DuckDB + ClickHouse +
  InfluxDB 3 + QuestDB + TimescaleDB adapters; result schema; first generated
  datasets; first local report (no aggressive claims yet); methodology + anti-Goodhart
  policy + hardware-reporting template + correctness rules.
- **Month 2 — Public API & observability:** minimal server; REST; **ILP ingest**;
  Prometheus; OTel; JSON/Parquet bench output; p50/p95/p99 reporting; cold/warm
  separation; GPU timing breakdown.
- **Months 3–4 — Hot-path optimization:** physical value types; integer-timestamp
  path; batch-ingest rewrite; minimized `BigDecimal` hot-loop use; typed arrays;
  segment/page statistics; columnar segment prototype; nightly benchmark-regression CI.
  *Goal: establish whether storage or conversion is the dominant bottleneck.*
- **Months 4–5 — GPU flagship:** command batching; true async handles; CPU/GPU
  overlap; hardware auto-tuning; end-to-end interpolation benchmarks; CPU-only vs GPU
  cost/performance report; portability/determinism matrix. *Publish only if strong and
  reproducible.*
- **Month 5 — Compression v2 prototype:** timestamp delta/delta-of-delta; scaled-int
  compression; f64 codec prototype; random-access blocks; model-based prototype;
  accuracy benchmarks.
- **Month 6 — Commercial beta:** Docker image; API keys/TLS; backup/restore MVP;
  Python SDK; Grafana dashboard; benchmark report; customer-runnable DSP-Bench;
  design-partner onboarding docs. *Goal: a "DSP Performance Preview" for 3–5 partners.*

---

## Benchmark report template

Every public report includes:

- **Hardware:** CPU (model/cores/threads), RAM, disk + filesystem, GPU + memory, PCIe
  gen, OS/kernel, driver versions, cloud instance + hourly cost.
- **Systems:** every system's version/git SHA/config (DSP, ClickHouse, InfluxDB 3,
  QuestDB, TimescaleDB, DuckDB, IoTDB; optional kdb+ if legally permitted).
- **Datasets:** regular + irregular IoT, high-frequency finance, scientific sensor,
  long single series, many short series, sparse/missing, block missingness,
  compression corpus, customer-provided (anonymized, where allowed).
- **Workloads:** ingest, online ingest/query, raw range fetch, point lookup,
  aggregation, downsampling, interpolation, gap filling, compression, compressed
  query, analytics pipeline.
- **Metrics:** points/sec, bytes/sec, p50/p95/p99/max latency, storage bytes/point,
  compression ratio + compress/decompress speed, random-access latency, GPU
  upload/kernel/readback, memory, CPU + GPU utilization, freshness lag, cost estimate,
  correctness/error metrics.
- **Required honesty section:** where DSP wins, where it loses, workloads not tested,
  known limitations, config caveats, reproduction instructions.

---

## Business validation

**Target personas (feel interpolation/resampling pain):** quant finance / market data;
industrial IoT / SCADA; energy / grid monitoring; scientific instrumentation;
robotics / autonomous telemetry; healthcare / wearables (if compliance scope is
manageable).

**Design-partner criteria:** irregular data; large historical ranges; production
interpolation/gap filling; pain with current TSDB/app-layer workflow; measurable
latency/cost targets; willing to run DSP-Bench and share anonymized results. *Avoid
partners who only need generic dashboards.*

**Pricing hypotheses:** open-source core; paid enterprise server; paid GPU
acceleration; paid advanced compression; paid security/compliance; paid benchmark
consulting; managed DSP-Bench reports; support subscription. **Validate willingness to
pay before building expensive distributed features.** Key question: *is interpolation
performance a budget-owning pain, or merely an engineering annoyance?* If unclear, run
customer discovery before heavy multi-node/GPU-storage investment.

---

## Legal & licensing

Before publishing comparative results: review each competitor's benchmark-publication
terms; be careful with kdb+ licensing; disclose configs; avoid misleading comparisons;
include reproduction instructions; check dependency + **compression-codec patents/
licenses**; document trademark usage; clarify whether benchmark data may be
redistributed. *For commercial trust, be more transparent than competitors.*

---

## Immediate next actions

1. ✅ Create `dsp-bench` as a first-class workspace member. *(Scaffold landed:
   library crate wired into the workspace, builds + tests green.)*
2. ✅ Define the first benchmark profile: `interpolation-heavy-irregular`.
   *(Seeded, reproducible dataset generator + workload profile in
   `dsp-bench/src/profile.rs`.)*
3. 🟡 Add DSP and DuckDB adapters. *(DSP adapter implemented against the
   vendor-neutral `SystemAdapter` trait, driving `splimes::auto_interpolate`. Two
   portable in-process baselines now stand beside it: **`BaselineLinearAdapter`**
   (`dsp-bench/src/baseline_adapter.rs`) — the fair-protocol class-(C) client-side
   linear baseline — and **`ForwardFillAdapter`**
   (`dsp-bench/src/forward_fill_adapter.rs`) — the class-(B) portable mirror of
   native TSDB gap-fill (`FILL(previous)`/`locf()`). The CLI's `--compare` flag now
   runs the full baseline suite, so reports carry a real three-system comparison
   (`dsp` vs `baseline-linear` vs `baseline-forward-fill`). The three methods are
   now compared on **quality, not only speed**: `dsp-bench/src/accuracy.rs`
   (`AccuracyMetrics`: RMSE/MAE/max-error/bias) scores each reconstruction against
   the synthetic profile's **known analytic ground truth** (fair-protocol Phase
   1.2 / Phase 6.4), carried as `BenchResult.accuracy` and surfaced by
   the CLI `--synthetic` mode (with `--seed`/`--points`/`--missingness`/
   `--jitter`/`--noise`/`--shape` knobs). The synthetic ground truth is now
   **shape-selectable** — `SignalShape::{MultiSine, Sawtooth, Step, DampedSine}`
   (`dsp-bench/src/profile.rs`), each recorded in the artifact (schema v5:
   `dataset.signal_shape`) so a result regenerates exactly from seed + knobs +
   shape. Honest findings it exposes depend on the shape: on the smooth
   high-frequency `multisine` (and on `step`) DSP's cubic spline leads on RMSE,
   but on the `sawtooth` the portable linear baseline out-accuracies the cubic
   (which overshoots the sharp discontinuities) — surfaced, not hidden. The
   external-engine DuckDB adapter — a real database baseline — is still to do.)*
4. Add ClickHouse, InfluxDB 3, QuestDB, TimescaleDB adapters.
5. ✅ Implement InfluxDB Line Protocol ingest. *(ILP **format parser** lives in
   the shared, vendor-neutral **`dsp-line-protocol`** crate (extracted from
   `dsp-bench` so the harness and the server speak one dialect): `parse` →
   `LineRecord`s and `parse_points` → sorted `splimes::Point`s for a chosen
   numeric field, with full tag/typed-field/escape/comment/precision handling
   and no vendor deps — the TSBS-compatibility on-ramp. **Wired end-to-end
   through a workload profile:** `DatasetSource::{Generated, LineProtocol}` +
   `InterpolationProfile::from_line_protocol` in `dsp-bench/src/profile.rs` drive
   the same interpolation harness, correctness gate, and JSON report from a real
   `.lp`/TSBS payload. **Runnable from disk** via the `dsp-bench` binary. **And
   now a server endpoint:** `POST /api/v1/interpolate/ilp` in the new
   **`dsp-server`** crate ingests an ILP payload (body = `text/plain`,
   field/precision/spline/resolution as query params) and interpolates it
   through the shared engine path — the Phase-2 server-side ILP ingest endpoint.)*
6. 🟡 Add end-to-end timing spans. *(Harness-level spans landed:
   `TimingBreakdown` in `dsp-bench/src/schema.rs` records dataset-generation
   cost, the summed measured adapter calls, and the whole-run span, wired into
   `run_profile` → `BenchResult.timing` (schema v3, back-compatible). Deeper
   per-pipeline-stage spans belong to the instrumentation track.)*
7. ✅ Add p50/p95/p99 + confidence-interval reporting. *(p50/p95/p99 +
   min/max/mean/stddev and seeded **bootstrap confidence intervals**
   (`LatencyStats::bootstrap_cis`, wired into `run_profile` →
   `BenchResult.latency_ci`) landed in `dsp-bench/src/stats.rs`.)*
8. ✅ Add physical value types for at least `F64`, `ScaledI64`,
   `BigDecimalText`. *(Delivered and exceeded: the vendor-neutral
   **`dsp-physical-type`** crate ships all six Phase-4.1 `PhysicalType`
   encodings — `F64`, `F32`, `ScaledI64`, `ScaledI128`, `Decimal128`,
   `BigDecimalText` — each with `encode` reporting an explicit `Exactness`
   (Exact / Lossy-with-residual-error / never-silent, per hard constraint #4),
   an always-available `to_logical` inverse, a declarative `PhysicalProfile`
   (storage width + lossless/hot-path eligibility), a columnar `encode_column`
   (aggregate exactness + `estimated_bytes` for bytes/point), and an advisory
   `recommend_encoding` that picks the narrowest hot-path encoding within an
   error tolerance. `BigDecimal` remains the logical/API type. 27 tests; 0
   clippy warnings under pedantic+nursery.)*
9. ✅ Prototype columnar segment reads for one aspect type. *(Delivered:
   `database::SegmentStore` seals an aspect's batches to typed columnar
   `.dspseg` files (single-block and paged) and reads them back through the
   libSQL `SegmentIndexStore` — `read_time_range` prunes segments by time in
   SQL and opens only the overlapping files (paged frames skip pages within a
   file too), `read_value_range` prunes by value via the resident
   `SegmentIndex`. See Phase 4.3.)*
10. Publish a methodology document **before** any performance claim.

> The key commercial move is not adding features — it is making DSP's performance
> claims measurable, reproducible, and valuable to a specific buyer. If DSP can
> credibly show it is faster, cheaper, or more accurate for large-scale interpolation
> and compression-aware irregular time-series analysis than general-purpose TSDBs, it
> has a clear path to a commercial product.

---

*Strategic spine merged from a peer-reviewed commercial/benchmark analysis; concrete
backlog distilled from the archived [`legacy/`](legacy) predecessor repositories
(retained for reference + full history, excluded from the active Cargo workspace).*
