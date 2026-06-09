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
  behavior.
- **4.2 Timestamp semantics:** integer epoch internally (ns/µs as needed), explicit
  tz + leap-second policy, monotonic ordering; delta / delta-of-delta / bit-pack / RLE.
- **4.3 Columnar segment store** (`AspectStorageMode::{LibSqlRows, SegmentedColumnar,
  Hybrid}`): append-friendly, immutable-after-seal, compactable, checksummed,
  page-indexed, random-access. Layout: `catalog.db` + per-aspect `metadata.db`,
  `segments/*.dspseg`, `segment_index.db`. Per-segment: timestamp/value/quality
  columns, physical type, codec, min/max ts + value, row/null counts, page offsets,
  per-page stats, checksums, version. Arrow-compatible memory internally; Parquet
  import/export from day one — a custom `.dspseg` is justified only for
  interpolation/random-access performance.
- **4.4 Data skipping** (time/value/tag/quality pruning, page skipping).
- **4.5 Arrow-compatible arrays** (eases Python/Flight/DataFusion/Parquet).
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
| B-rest | **REST facade** + **declarative query-params** (`range`/`take`/`count`/`page`/`interpolation`) + **pagination**. | 🔴/🟡 | 2 | `DSM-Database` |
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
   vendor-neutral `SystemAdapter` trait, driving `splimes::auto_interpolate`;
   DuckDB adapter still to do.)*
4. Add ClickHouse, InfluxDB 3, QuestDB, TimescaleDB adapters.
5. 🟡 Implement InfluxDB Line Protocol ingest. *(ILP **format parser** landed in
   `dsp-bench/src/line_protocol.rs`: `parse` → `LineRecord`s and `parse_points`
   → sorted `splimes::Point`s for a chosen numeric field, with full
   tag/typed-field/escape/comment/precision handling and no vendor deps — the
   TSBS-compatibility on-ramp. **Now wired end-to-end through a workload profile:**
   `DatasetSource::{Generated, LineProtocol}` + `InterpolationProfile::from_line_protocol`
   in `dsp-bench/src/profile.rs` drive the same interpolation harness, correctness
   gate, and JSON report from a real `.lp`/TSBS payload (timestamp bounds derived
   from the data). Still open: the server-side ILP ingest **endpoint** (Phase 2).)*
6. 🟡 Add end-to-end timing spans. *(Harness-level spans landed:
   `TimingBreakdown` in `dsp-bench/src/schema.rs` records dataset-generation
   cost, the summed measured adapter calls, and the whole-run span, wired into
   `run_profile` → `BenchResult.timing` (schema v3, back-compatible). Deeper
   per-pipeline-stage spans belong to the instrumentation track.)*
7. ✅ Add p50/p95/p99 + confidence-interval reporting. *(p50/p95/p99 +
   min/max/mean/stddev and seeded **bootstrap confidence intervals**
   (`LatencyStats::bootstrap_cis`, wired into `run_profile` →
   `BenchResult.latency_ci`) landed in `dsp-bench/src/stats.rs`.)*
8. Add physical value types for at least `F64`, `ScaledI64`, `BigDecimalText`.
9. Prototype columnar segment reads for one aspect type.
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
