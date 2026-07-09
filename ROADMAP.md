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

**This file is the single source of truth for status, priorities, and to-dos.**
Every work item is a `[ ]` (to do) or `[x]` (done) checkbox. Shipped capability is
*described* in [`README.md`](README.md) (the consumer-facing features document) —
here it is only ticked. There is no run log: git history and PRs are the record of
what happened. The strategic spine merges a peer-reviewed commercial/benchmark
analysis with the feature backlog distilled from DSP's **15 predecessor
repositories** (archived under [`legacy/`](legacy) with full history).

---

## Table of Contents

- [Commercial thesis & positioning](#commercial-thesis--positioning)
- [Guiding principles & hard constraints](#guiding-principles--hard-constraints)
- [Top-level tracks](#top-level-tracks)
- [Phased roadmap (0–9)](#phased-roadmap-09)
- [Updated priority order](#updated-priority-order)
- [Predecessor-derived backlog](#predecessor-derived-backlog)
- [Control-plane engine: Turso/libSQL 0.6 adoption](#control-plane-engine-tursolibsql-06-adoption)
- [Six-month execution plan](#six-month-execution-plan)
- [Benchmark report requirements](#benchmark-report-requirements)
- [Research & business notes](#research--business-notes)
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

- [ ] Ship honesty pages: *When DSP beats general TSDBs* · *When DSP is not the
  right tool* · *Benchmark methodology* · *Interpolation-accuracy methodology* ·
  *GPU tuning & economics* · *Precision & physical value types*.

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
3. **Storage boundary.** **libSQL/Turso is the control plane** — catalog, metadata,
   config, pipeline state, transactional control. DSP's own **typed columnar
   measurement segments** (Storage v2) own the high-volume measurement hot path.
   External databases like InfluxDB are *integration targets* (source and/or sink)
   reached through connectors — **never** replacement backends. Do not reintroduce
   `sled` or `sqlx`.
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

Priority order:

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

### Phase 0 — Benchmark thesis & buyer wedge · *Immediate*

**Acceptance:** you can answer "why choose DSP over InfluxDB/QuestDB/ClickHouse/
TimescaleDB/IoTDB/DuckDB/kdb+?"; 3–5 design partners with interpolation-heavy
workloads identified; willingness-to-pay hypotheses documented.

- [x] Public performance thesis + first benchmark-claim list *(the [Commercial thesis](#commercial-thesis--positioning) section)*
- [x] Define what DSP is **not** chasing first
- [ ] Identify 3–5 design partners with interpolation-heavy workloads
- [ ] Document willingness-to-pay hypotheses

### Phase 1 — Build `DSP-Bench` · *Highest priority*

A `dsp-bench` workspace that is both an internal engineering suite and a
public/customer-runnable diagnostic. Model it on SEER/TSM-Bench (result storage,
dashboards, repeatable config, system adapters, workload profiles) — not ad-hoc
scripts. Implement **TSBS compatibility** (via InfluxDB Line Protocol) for baseline
comparisons, but make DSP-Bench the primary suite. Target layout: `adapters/`
(dsp, clickhouse, influxdb3, questdb, timescaledb, iotdb, duckdb, kdb_optional),
`workloads/` (ingest, online_ingest_query, range_fetch, point_lookup, downsample,
upsample_interpolate *(flagship)*, gap_fill, compression, compressed_query,
pattern_pipeline), `datasets/`, `runners/` (local, docker_compose, cloud),
`reports/` (json, parquet, html, grafana), `dashboards/`, `docs/`.

- [x] `dsp-bench` first-class workspace member (builds + tests green)
- [x] First workload profile: `interpolation-heavy-irregular` (seeded, reproducible generator)
- [x] DSP adapter over the vendor-neutral `SystemAdapter` trait (drives `splimes`)
- [x] Portable baselines — linear (fair-protocol class C) + forward-fill (class B)
- [x] Accuracy scoring vs analytic ground truth (RMSE/MAE/max-error/bias)
- [x] Shape-selectable synthetic truth (`MultiSine`/`Sawtooth`/`Step`/`DampedSine`)
- [x] Latency stats — p50/p95/p99 + min/max/mean/stddev + bootstrap CIs
- [x] Harness-level timing breakdown (`BenchResult.timing`)
- [x] JSON report output
- [x] HTML report output (`--html`, self-contained) + best-effort hardware probe (CPU model/cores/RAM via `sysinfo`)
- [x] Storage estimate per result (`StorageEstimate`: recommended encoding + value/timestamp/total bytes/point)
- [x] CLI runner — `.lp`/TSBS input or `--synthetic` (seed/points/missingness/jitter/noise/shape knobs); non-zero exit on correctness failure
- [ ] External-engine adapters — DuckDB first, then ClickHouse, InfluxDB 3, QuestDB, TimescaleDB (+ IoTDB)
- [ ] Full TSBS-compatible comparison harness (ILP parser shipped; harness pending)
- [ ] Additional workloads — bulk-ingest growth curves (1M→10M→100M→1B), online ingest+query, raw range fetch, point lookup, downsample (min/max/avg/count, OHLC, TWA, percentiles), gap fill, compression, compressed query, analytics pipeline
- [ ] Fair-protocol depth (Phase 1.1) — ≥10 reps for short tests, cold/warm/hot/post-compaction/post-restart separation, saturation curves (batch size, clients, writers, query concurrency, cardinality, dataset size, GPU output size), seeded randomized query mixes (published seeds), failure tests (restart during ingest, crash during compaction, network retry, partial/corrupt segment), independent-reproducibility packaging (versions, SHAs, images, configs, hardware, drivers, command lines, raw artifacts)
- [ ] Fair interpolation comparisons (Phase 1.2) — report three classes where possible: (A) native in-DB (Timescale gapfill, QuestDB `SAMPLE BY … FILL`, InfluxQL/SQL fill, ClickHouse ASOF/window, DuckDB window fns, IoTDB fns); (B) portable SQL baseline; (C) client-side end-to-end. Don't hide unfavorable results.
- [ ] Remaining hardware capture — disk, GPU, and driver versions in run metadata
- [ ] Report surfaces beyond JSON/HTML — Parquet / Grafana dashboards
- [ ] Anti-Goodhart (Phase 1.3) — publish negative results + DSP-losing workloads; benchmark code separate from engine code; run customer-supplied workloads; README policy line *("DSP benchmarks guide real engineering decisions, not synthetic wins")* — policy line + the sawtooth negative finding shipped; the publication pipeline is open

### Phase 2 — Benchmark-grade server/API · *Very high*

A commercial DB can't lead with an embedded Rust API + TUI. `dsp-server` (axum) is
that surface — see README ["HTTP Server"](README.md#http-server) for what ships.
Interop priority: REST/JSON → ILP → Arrow/Parquet import-export → Prometheus
metrics → OpenTelemetry → Python SDK → Rust SDK → Arrow Flight/Flight SQL → SQL
surface / DataFusion (later) → Grafana → Prometheus remote write/read (if
monitoring) → R/Arrow workflows.

**Acceptance:** DSP-Bench drives DSP entirely through public APIs; the TUI becomes
secondary (demos/debugging); API responses distinguish raw / interpolated /
extrapolated / compressed / reconstructed values *(delivers the synthetic-point
marking nuance — backlog item B-tags)*.

- [x] `axum` server skeleton — `/health`, `/ready`, Prometheus `/metrics`
- [x] Flagship `POST /api/v1/interpolate` over the `splimes` engine (raw/interpolated/extrapolated provenance on every point)
- [x] InfluxDB-Line-Protocol ingest — `POST /api/v1/interpolate/ilp` (+ downsample sibling)
- [x] Downsample/aggregation query — `POST /api/v1/downsample` (+ `…/ilp`; `BigDecimal` reductions, epoch-grid-aligned buckets)
- [x] Single-instant point query — `POST /api/v1/interpolate/point` (raw/interpolated/extrapolated labelling)
- [x] Stored-range read surface — Arrow IPC `…/storage/{aspect}/range` + `…/value-range`, JSON `…/points` + `…/value-points`, `…/storage/aspects` / `…/{aspect}/schema` / `…/stats` (bytes/point)
- [x] Single-instant read surface — `GET …/storage/{aspect}/at?t=` point lookup (order-signal-driven read planner: binary search on a sorted segment, linear scan on an out-of-order one)
- [x] Storage maintenance — `POST …/storage/{aspect}/reconcile` (in-place out-of-order reconciliation pass; Phase 4.6)
- [x] HTTP catalog management + ingest — declare aspect schema; JSON/ILP/Parquet/CSV batch ingest; `…/storage/catalog`; no-silent-downcast enforced (`400` on unrepresentable); `dsp_ingest_*` counters
- [x] B-rest pagination — `offset`/`limit`/`take`/`page` + `total`/`count` on `…/points` and `…/value-points`
- [x] Parquet interchange — `…/range.parquet` / `…/value-range.parquet` export + `POST …/parquet` ingest
- [x] CSV interchange — stored-range export/ingest + compute-endpoint CSV output
- [x] Columnar output for compute endpoints — Arrow IPC + Parquet for `interpolate`/`downsample` (+ every ILP sibling); `dsp-arrow` reconstructed-series + reduction-table interchange
- [x] Prometheus latency histograms — compute endpoints, ILP compute path, and the storage-ingest seal path
- [x] OpenTelemetry trace export (paired with Prometheus `/metrics`) — env-gated OTLP/gRPC exporter beside the `fmt` subscriber (delivery to a live collector = a runtime follow-up)
- [x] B-rest residue — `interpolation` query-param alias for `spline` on the ILP compute endpoints (`spline` wins when both given)
- [x] B-rest residue — cursor paging (stable forward-iteration token) on `…/points` / `…/value-points`: an opaque `next_cursor` (hex position token over the deterministic read order) + `?cursor=` (supersedes `offset`/`page`, malformed → 400); a client follows `next_cursor` until absent
- [ ] Python SDK → Rust SDK → Arrow Flight / Flight SQL → SQL surface / DataFusion (later)
- [ ] Grafana → Prometheus remote write/read (if monitoring) → R/Arrow workflows

### Phase 3 — Instrument everything · *Very high*

Before optimizing, make bottlenecks visible. **Acceptance:** for any slow benchmark
you can say where the time went (e.g. "52% value parsing, 18% segment read, 15% GPU
transfer, 8% kernel, 7% JSON").

- [x] Prometheus `/metrics` surface
- [x] Harness-level timing breakdown (dataset-gen + adapter + whole-run spans)
- [x] Latency histograms over the compute + seal paths (`/metrics`)
- [x] `/debug/profile/current` — live p50/p95/p99 latency snapshot
- [x] Tracing foundation — a `RUST_LOG`-driven `fmt` subscriber (span-CLOSE events, busy/idle timing) with per-request engine spans on the flagship compute path: `interpolate.engine` (input/output points, spline, resolution) and `downsample.reduce` (candidate/in-window points, buckets, resolution)
- [x] Per-request **root span** + per-stage child spans (partial coverage of ingest → serialization): a `request{method,path,request_id}` span (via `axum::middleware::from_fn`, `x-request-id` in/out) that every stage span nests under — `interpolate.parse`/`interpolate.compute`/`interpolate.serialize` under `interpolate.engine`, `downsample.parse` beside `downsample.reduce`, and `storage.ingest.parse`/`storage.ingest.seal` on the write path (the seal span surfaces the nested Turso `connect_with_encryption` control-plane spans)
- [x] Per-stage spans on the **storage read path** (`storage.{range,value_range,point}.read`
  + `.serialize`, uniform `format` dimension over JSON/CSV/Arrow/Parquet), the **ingest
  decode paths** (`storage.ingest.parse`/`normalize`/`seal` for ILP + CSV + JSON), and
  the **reconcile daemon** (`reconcile.tick{kind,...,aspects,segments}` on all five tick
  kinds) — all runtime-verified against the running binary
- [x] Arrow/Parquet decode on ingest — `storage.ingest.parquet` stage span (byte length + sort guard + `format="parquet"`), nesting under the request root span; runtime-verified against the running binary
- [ ] Remaining per-stage spans — auth · WAL append · explicit libSQL write · commit · index update · page skip · cache hit/miss · decompression · CPU interp · GPU upload/queue/kernel/readback
- [x] **OTLP trace export** (pairs with `/metrics`): shipped — env-gated on
  `OTEL_EXPORTER_OTLP_ENDPOINT`, an OTLP/gRPC `SdkTracerProvider` (batch exporter +
  `dsp-server` service resource) with a `tracing_opentelemetry` layer beside the `fmt`
  subscriber; the shipped request/stage spans export to a collector. Dep constellation
  confirmed current on crates.io (May 2026): `opentelemetry` 0.32 + `opentelemetry_sdk`
  0.32 (`rt-tokio`) + `opentelemetry-otlp` 0.32 (`grpc-tonic`, builds protoc-free) +
  `tracing-opentelemetry` 0.33. A misconfigured/absent collector does not block startup.
- [x] **OTLP collector verification: delivery verified.** A Jaeger all-in-one collector
  (docker `jaeger`, OTLP/gRPC :4317, query API/UI :16686, `--restart unless-stopped`)
  now runs on the dev box; `scripts/verify-otlp.sh` boots `dsp-server` against it,
  drives a declare/ingest/read cycle, and asserts via the Jaeger query API that
  `request` root spans land with their nested stage spans
  (`storage.ingest.parse`/`seal`, `storage.range.read`/`serialize` observed nesting
  correctly). Re-runnable headlessly; the script (re)starts the container if absent.
- [ ] `Statement::n_change()` write accounting (Turso 0.6) in ingest/instrumentation spans
- [ ] `/bench/runs/:id` endpoint

### Phase 4 — Hot path: physical types & Storage v2 · *High*

**Acceptance:** range scans avoid materializing `BigDecimal` unless requested;
interpolation streams from typed arrays; page/segment pruning visible in debug output;
DSP-Bench shows ingest/scan/compression gains; correctness tests cover late + OOO data.

- [x] **4.1 Physical numeric encodings** — `dsp-physical-type`: all six `PhysicalType`s
  (`F32`/`F64`/`ScaledI64`/`ScaledI128`/`Decimal128`/`BigDecimalText`) with explicit
  `Exactness`, `PhysicalProfile`, columnar `encode_column`, advisory
  `recommend_encoding`, and schema-level declaration + enforced seal
  (`AspectSchema::seal`, `SealError::{Encode,ToleranceExceeded}`)
- **4.2 Timestamp semantics**
  - [x] Integer-epoch `TimeUnit` (s/ms/µs/ns) with lossless delta + delta-of-delta transforms, zig-zag + LEB128 varint estimate, RLE, and `best_estimated_bytes` selector
  - [x] Fixed-width bit-packing codec — **realized on disk**: the `.dspseg` timestamp block writes a self-describing codec selector (bit-pack vs varint second differences), so the bytes/point saving is stored, not just estimated (segment format v3, paged v4)
  - [x] Monotonic-order enforcement — `first_order_violation` primitive; opt-in `Segment::build_sorted`/`build_nullable_sorted`, `AspectSchema::seal_sorted`/`seal_paged_sorted` (`SegmentError`/`SealError::OutOfOrder`); exposed at the API as `require_sorted` on **all four** ingest formats (JSON/CSV/ILP/Parquet); observability via `SegmentStats::time_sorted` → per-segment index → `unsorted_segments` in aspect/store stats + `time_sorted` on every ingest response
  - [ ] Explicit tz + leap-second policy
  - [x] Out-of-order **reconciliation** (Phase 4.6) — enforcement/detection, in-place per-segment reconciliation, threshold + hot/cold background sweeps, and the **cross-segment overlap merge** (newer-wins dedup) all shipped; the split-not-rewrite optimization + composite upsert keys are the residue (see 4.6)
- [x] **4.3 Columnar segment store** — in-memory `Segment`/`PagedSegment`; versioned
  CRC-checksummed `.dspseg` frames (v1 single-block, v2 null/quality column, v3 paged
  with per-page index); `AspectSchema::seal_paged[_nullable]` (per-page tolerance
  enforcement, global-row error remap); `SegmentIndex`/`SegmentIndexStore` (libSQL
  control plane); `SegmentStore` (seal + prune-then-open reads + `aspect_stats`);
  `CatalogStore`/`AspectCatalog` hierarchy + schema-aware declare-once/seal-by-lookup;
  per-aspect `metadata.db` rollups (`AspectMetadataStore`, O(1) stats, rebuildable
  from the index)
- **4.4 Data skipping**
  - [x] Segment-level time/value pruning (`prune_by_time`/`prune_by_value`, conservative `may_contain_value`)
  - [x] Quality pruning (`prune_present_by_time`, all-null segment/page skipping)
  - [x] Intra-segment page skipping (`prune_pages_by_time`, decode only surviving pages)
  - [ ] Tag pruning *(blocked on B-tags — per-measurement tags/labels don't exist yet)*
- [x] **4.5 Arrow-compatible arrays** — `dsp-arrow` + `dsp-arrow-store`: lossless +
  typed-fast-path `RecordBatch` interchange (exact `Decimal128` for scaled ints),
  paged-batch streams, logical-column builders, Arrow IPC stream bytes, Parquet
  import/export; `arrow-*` tree confined to the leaf crates
- [ ] **4.6 Correctness semantics** — out-of-order/late data, dedup, upsert, idempotent
  batch ingest, clock skew, precision, tz parsing, leap seconds, query consistency
  during compaction, read-your-writes, snapshot isolation
  - [x] Out-of-order **detection + enforcement** (shipped in 4.2): `require_sorted`
    rejects OOO batches; `unsorted_segments` counts segments needing reconciliation
  - [x] Out-of-order **reconciliation — in-place per-segment sort** (first slice):
    `SegmentStore::reconcile_segment`/`reconcile_aspect` stable-sort an out-of-order
    segment's rows by timestamp and re-seal them sorted at the same id/file (frame
    kind preserved), dropping it out of `unsorted_segments` so a point lookup over it
    binary-searches; exposed at the API as `POST /storage/{aspect}/reconcile`
  - [x] Out-of-order **reconciliation — cross-segment merge** (baseline): detect
    time-overlapping segments (`SegmentIndex::overlapping_count`/`overlapping_segments`)
    and collapse each connected overlap component into one time-sorted segment with
    **newer-wins** (last-writer-wins / upsert) dedup on shared timestamps
    (`merge_newer_wins` + `SegmentStore::reconcile_overlaps`/`reconcile_all_overlaps`,
    `SegmentIndexStore::delete`); exposed at the API as `?overlaps=true` on the
    per-aspect and store-wide reconcile endpoints and as a background daemon sweep
    (`DSP_RECONCILE_OVERLAPS`). This matches QuestDB's DEDUP UPSERT "last write wins on
    the designated timestamp" semantics. *(src: last-write-wins dedup on designated
    timestamp + upsert keys — https://questdb.com/docs/concepts/deduplication/)*
  - [x] Out-of-order **reconciliation — split-not-rewrite optimization**: shipped.
    `SegmentStore::split_segment` carves a sorted segment into a cold prefix (kept id) +
    hot suffix (new id) via `split_index`; `reconcile_overlaps_with_policy` consumes
    `SplitPolicy::decide` so an overlap component whose cold prefix clears the floor and
    outweighs its hot suffix is split rather than fully rewritten (default
    `reconcile_overlaps` keeps the QuestDB 50 MiB floor, so production behaviour is
    unchanged); exposed as `?split_min_bytes=` on the per-aspect + store-wide reconcile
    endpoints and the `DSP_RECONCILE_SPLIT_MIN_BYTES` daemon env. The **squash** half is
    shipped too — `squash_aspect`/`squash_aspect_if_exceeds` (QuestDB-`max.splits`-style
    trigger) + `squash_all_over_threshold`, `POST …/{aspect}/squash?max_segments=`, and
    the `DSP_RECONCILE_MAX_SPLITS` daemon env fold over-fragmented aspects back to one
    segment. *(src: https://questdb.com/docs/concepts/partitions/)*
  - [ ] Out-of-order **reconciliation — configurable upsert keys + skip-identical**:
    the merge dedups on the timestamp alone (newer wins); add optional composite
    dedup/upsert keys (timestamp + declared columns) and a skip-write when the newer
    row is byte-identical to the older, matching QuestDB's `DEDUP UPSERT KEYS`. Blocked
    on B-tags (per-measurement columns/tags don't exist yet). *(src: composite
    UPSERT KEYS, identical-row skip — https://questdb.com/docs/concepts/deduplication/)*
  - [x] **Threshold-triggered background reconciliation**: the `unsorted_segments`
    count drives a QuestDB-`max.splits`-style trigger for an automatic background
    reconcile pass (single-aspect `reconcile_aspect_if_unsorted_exceeds` + store-wide
    `reconcile_all_over_threshold` sweep), exposed as the `?threshold=N` gate on
    `POST …/{aspect}/reconcile`, a manual store-wide `POST …/storage/reconcile`, and
    a timer daemon (`DSP_RECONCILE_INTERVAL_SECS`/`DSP_RECONCILE_THRESHOLD`) with the
    `dsp_reconcile_passes_total`/`dsp_reconcile_segments_reconciled_total` metrics
  - [x] **Hot/cold split in the background reconcile**: cold (non-hot-tail) segments
    are reconciled every pass and only the hot tail (most-recently-sealed) is deferred
    until the backlog reaches the threshold — `SegmentStore::reconcile_aspect_hot_cold`/
    `reconcile_all_hot_cold`, wired through the reconcile daemon (`DSP_RECONCILE_HOT_COLD`)
    and the `?hot_cold=true` reconcile endpoints; matches QuestDB's "squash non-active
    partitions each commit, defer the active partition until the split threshold".
    *(src: non-active squashed each commit, active squashed past the split threshold —
    https://questdb.com/docs/concepts/partitions/)*
  - [x] Read-planner: use the persisted per-segment `time_sorted` to binary-search a
    point lookup on a sorted segment and linear-scan only an out-of-order one
    (`Segment::value_at`/`PagedSegment::value_at`/`SegmentStore::read_point`; exposed
    as `GET /storage/{aspect}/at`)

### Phase 5 — GPU interpolation flagship · *High (parallel w/ Phase 4)*

Interpolation/extrapolation already works end to end (CPU/SIMD/GPU); this phase makes
the GPU path *fast end-to-end and economically justified*. GPU benchmarks must be
**end-to-end** (storage read → decode → filter → Decimal convert → transfer → queue
wait → kernel → readback → serialize); a kernel-only speedup is not commercially
credible.

**Acceptance:** publishable claim like *"on hardware X, DSP produces Y M interpolated
points/sec for irregular cubic interpolation including storage read, decode, GPU
transfer, kernel, readback, and API serialization, p95 = Z."*

- [x] Interpolation/extrapolation capability — linear/quadratic/cubic/polynomial on CPU, SIMD, and GPU (`Outputs::analyze_range`/`analyze_point`)
- [x] GPU infra — size-tiered LRU buffer pool (wired through the static f64/f32 paths), persistent staging buffers, async-handle scaffold (`GpuInterpolationResult<T>` + `IntoFuture`; computes synchronously today), `GpuConfig` presets + `prewarm_gpu_with_config()`/`gpu_buffer_pool_stats()`
- [ ] 5.1 Command batching
- [ ] 5.2 True async GPU handles (non-blocking; CPU parse/read overlaps GPU)
- [ ] 5.3 CPU/GPU overlap (chunked read → decode → upload → kernel → stream output)
- [ ] 5.4 Hardware auto-tuning (calibrate CPU/GPU throughput, transfer cost, break-even sizes; auto-select)
- [ ] 5.5 GPU economics (cloud cost, CPU-only fallback, utilization/contention/cold-start, NVIDIA/AMD/Intel/Apple portability)
- [ ] 5.6 wgpu/WGSL portability & determinism (conformance matrix, numerical drift, published tolerances)
- [ ] 5.7 Multi-GPU *(deferred until single-GPU wins are proven)*
- [ ] Memory-mapped GPU I/O *(only after transfer bottlenecks are measured)*
- [ ] End-to-end GPU interpolation benchmark (storage read → … → API serialization, with p95)
- [ ] GPU memory-stability + pool-eviction benchmark (verify stable GPU memory across repeated interpolation calls; pool statistics under large/small batches)

### Phase 6 — Compression v2 · *High*

**Acceptance:** honest claim like *"this mode cuts storage by X while preserving event
detection within Y% and improving historical query latency by Z."*

- [ ] **6.1 Lossless typed codecs** — timestamp delta/delta-of-delta + **fixed-width
  bit-packing** + **RLE** + **Gorilla variable-length** + **per-block adaptive
  bit-packing** *(all five realized on disk in the `.dspseg` timestamp block, chosen
  per-stream by `best_encoding_name`)*; **scaled-int value bit-packing** *(realized on
  disk in the value block)*; Chimp-style f64; ALP-inspired vectorized f64;
  Decimal128/scaled-int codecs; **block-level random access**. *(Check codec
  patents/licenses before embedding.)*
  - [x] **Gorilla + RLE realized on disk** — the timestamp block now carries four
    codecs (varint/bit-pack/RLE/Gorilla) chosen by the single-source-of-truth
    `best_encoding_name`, so the reported codec always matches the bytes written; the
    Gorilla win shows in the dsp-bench bytes/point.
  - [x] **Evaluated** a Gorilla-style variable-length second-difference estimate
    (`gorilla_dod_bits`/`gorilla_bytes`/`DeltaOfDeltaColumn::gorilla_estimated_bytes` —
    advisory only, NOT wired into the realized varint-vs-bit-pack selector). Measured
    finding: on a 64-point mostly-regular stream with two isolated 50k-gap spikes,
    gorilla=52 B beats fixed-width bit-pack=143 B (the roadmap hypothesis holds vs
    bit-pack), but the **already-shipped RLE codec (32 B here) wins on isolated
    spikes**. Gorilla's unique advantage is therefore *scattered single* jitter (RLE
    can't form runs, bit-pack pays max width for the many near-zero values). *(src:
    Gorilla, VLDB'15)*
  - [x] **Adopt-or-drop decision for the Gorilla codec: ADOPTED.** Benchmarked gorilla
    vs varint/RLE/bit-pack on a scattered-single-jitter corpus: gorilla is the strict
    winner in its regime (e.g. 368 B vs the best shipped codec's 508 B, ~28% smaller,
    on a 1000-pt base with an isolated jitter every 16th interval within its ±2048
    bucket), with an honest loss boundary past ±2048 (falls to the 68-bit bucket).
    Realized as a full lossless codec (`encode_gorilla_dods`/`decode_gorilla_dods`),
    wired into the `.dspseg` block (`TS_CODEC_GORILLA`) + folded into
    `best_estimated_bytes`/`best_encoding_name`, and surfaced in the dsp-bench
    StorageEstimate. RLE was also realized on disk (`TS_CODEC_RLE`) in the same arc,
    closing its estimate/disk divergence. *(src: Gorilla, VLDB'15)*
  - [x] **Evaluated + ADOPTED dynamic (per-block adaptive) bit packing.** Measured on a
    256-value mixed-magnitude second-difference stream (one contiguous 32-wide window of
    ~30-bit values among ±1 narrow jitter, block=64): blocked=184 B is the strict winner —
    global bit-pack 961 B, varint 384 B, Gorilla 450 B, RLE 640 B (81% below global, 52%
    below the next-best shipped codec). Realized as the fifth timestamp codec
    (`blocked_bitpack_encode`/`decode`, `TS_CODEC_BLOCKED`, `BLOCKED_BITPACK_BLOCK=64`),
    folded into `best_estimated_bytes`/`best_encoding_name` (`delta_of_delta_blocked`),
    and round-trip- + runtime-verified. A single-block stream ties global bit-pack, so the
    selector keeps the simpler label there. *(src: "Dynamic Bit Packing", Sensors 2023 —
    https://www.mdpi.com/1424-8220/23/20/8575 · Sprintz per-block bit-packing, ACM TODS'18
    — https://arxiv.org/abs/1808.02515)*
  - [x] **Frame-of-Reference (FOR) per-block codec — prototyped + benchmarked (advisory).**
    `for_bitpack_bytes`/`encode`/`decode` beside the blocked codec: each block emits a
    zig-zag-varint reference (the block minimum) then the *unsigned* residuals (`v - min`)
    bit-packed at the block's *range* width, not its magnitude. Exposed as
    `DeltaOfDeltaColumn::for_estimated_bytes` (timestamp) and `ColumnEncoding::for_value_bytes`
    (value), and surfaced in the bench as `StorageEstimate.advisory_for_value_bytes` (schema
    v9). Benchmarked: timestamp dods on a clustered-offset corpus FOR=135 B vs per-block
    bit-pack 748 B (82% smaller); value mantissas clustered near 1e9 FOR=90 B vs blocked
    747 B / bit-pack 745 B / varint 960 B (88% smaller). *(src: Lemire, FOR+delta —
    https://lemire.me/blog/2012/02/08/effective-compression-using-frame-of-reference-and-delta-coding/
    · ALP FastLanes FOR, SIGMOD'24 — https://dl.acm.org/doi/10.1145/3626717)*
  - [ ] **FOR adopt-or-drop — OWNER DECISION (the metric-flip zone).** FOR is advisory only:
    NOT wired into `best_encoding_name`/`best_value_codec` or the `.dspseg` writer. Because
    FOR packs the residual *unsigned*, it beats zig-zag global/blocked bit-packing on *any*
    all-non-negative column (a 6-bit residual vs zig-zag's 7-bit width for `0..=63`), so
    adopting it would newly select `scaled_for` for a large fraction of columns and flip the
    realized `value_codec` / bytes-per-point — the same headline-metric semantic change
    already flagged in "Value-column realized-bytes accuracy" below. Decide adoption
    (timestamp: FOR rarely wins — dods are small/near-zero, so *recommend keep-advisory-or-drop
    for timestamps*; value: FOR wins the common clustered-high-base sensor regime, so the
    stronger adopt candidate) together with the bytes/point-flip sign-off.
  - [ ] **Sprintz-style FIRE predictor + zero-RLE (value + timestamp columns):** the
    realized per-block bit-pack is exactly Sprintz's bit-packing stage; add a Sprintz
    FIRE-style online integer forecaster to shrink residuals before packing and a
    run-length pass over the resulting zeros. Canonical low-resource IoT time-series
    compressor; benchmark the residual-bytes win on a real irregular corpus. *(src:
    Sprintz, ACM TODS'18 — https://arxiv.org/abs/1808.02515)*
  - [x] **Scaled-int value bit-pack codec — realized on disk.** The `.dspseg` value block
    now carries a self-describing codec selector (`VAL_CODEC_VARINT`/`VAL_CODEC_BITPACK`);
    a `ScaledI64` column whose mantissas fixed-width bit-pack below the per-value varint
    stores bit-packed (chosen via `ColumnEncoding::best_value_codec`), and the realized
    figure flows through `Segment::serialized_value_bytes()` +
    `StorageEstimate.realized_value_bytes`/`value_codec` (schema v8). Measured on a 480-pt
    exact 2-decimal ramp: bit-pack 901 B vs the prior varint 1440 B (37.4% smaller, 76.5%
    below the naive `len*8` estimate). Runtime-verified: a sealed `scaled_i64` aspect reads
    `bytes_per_point=2.13` through `/storage/{aspect}/stats`. *(validated by ALP's
    decimal→integer PseudoDecimal path, SIGMOD'24 — https://dl.acm.org/doi/10.1145/3626717)*
  - [ ] **Value-column realized-bytes accuracy (owner decision still open):** a realize-
    accurate `ColumnEncoding::serialized_bytes()`/`best_serialized_bytes()` +
    `Segment::serialized_value_bytes()` + `StorageEstimate.realized_value_bytes` (v7/v8)
    expose the true figure additively, and the value block now *realizes* the smaller codec
    on disk. **Owner decision needed:** whether to flip the headline `estimated_bytes` /
    `value_bytes` / `total_bytes_per_point` (and the bench HTML `val B/pt`) to the realized
    figure — a semantic change to a metric consumed in ~37 places / ~33 test assertions.
  - [x] **Per-block adaptive bit-pack for the VALUE column — realized on disk.** The
    `.dspseg` value block carries `VAL_CODEC_BLOCKED` (block-size uvarint + length-prefixed
    `blocked_bitpack_encode` stream), reusing the generic blocked primitives over the
    mantissa stream. `ColumnEncoding::blocked_value_bytes` folds into
    `best_serialized_bytes`/`best_value_codec` (`scaled_blocked`), chosen only when strictly
    smallest so regular/uniform columns are byte-for-byte unchanged; a non-`ScaledI64`
    payload is rejected (`value_codec_blocked_type`). Wins the mixed-magnitude regime (a
    quiet region + a wide burst straddling zero, where a global width over-pays and a FOR
    reference is wasted). Round-trip- + framed-segment-verified. *(src: "Dynamic Bit Packing",
    Sensors 2023 — https://www.mdpi.com/1424-8220/23/20/8575)*
  - [ ] **f64 value-column codec (distinct from the timestamp DoD codec):** prototype a
    **Chimp128**-style XOR codec — XOR each value against the best of the previous 128
    values (the one giving the most trailing zeros) rather than only the immediate
    predecessor (Gorilla/Chimp), since ~95% of adjacent XORs have ≤5 trailing zeros.
    Benchmark it against Gorilla/Elf/ALP on the `dsp-bench` bytes/point + throughput
    metrics using the FCBench / 2025-VLDB-eval methodology before adopting. *(src: Chimp,
    VLDB'22 — https://www.vldb.org/pvldb/vol15/p3058-liakos.pdf · comprehensive eval,
    VLDB'25 — https://www.vldb.org/pvldb/vol18/p4396-hishida.pdf · FCBench —
    https://arxiv.org/pdf/2312.10301)*
  - [ ] **FastLanes "Unified Transposed Layout" for the bit-pack codecs (decode-speed
    slice):** DSP's global/blocked/FOR bit-packers pack scalar LSB-first, so decode is a
    per-value bit loop. FastLanes reorders values into a *transposed* layout targeting a
    virtual 1024-bit SIMD register, decoding >100 B integers/sec with scalar (auto-vectorized)
    code — the layout that makes lightweight codecs cheap enough to always leave data
    compressed. Prototype the 1024-value transposed bit-unpack for the realized `ScaledI64`
    value codec and benchmark decode throughput (bytes/point is unchanged; this is a
    decode-latency win that also unlocks a future GPU-unpack path). *(src: FastLanes
    Compression Layout, VLDB'23 — https://www.vldb.org/pvldb/vol16/p2132-afroozeh.pdf)*
  - [ ] **Cascading (recursive) codec composition — FOR→delta→bit-pack chains:** DSP's value
    codecs are single-level (one of varint/bit-pack/blocked/FOR). FastLanes and Vortex apply
    codecs *recursively* (e.g. FOR reference, then delta, then bit-pack the residual), which
    is exactly the FOR-then-blocked-bit-pack cascade the advisory FOR estimate points at.
    Design a small codec-chain descriptor in the `.dspseg` value block so a column can carry
    a composed pipeline instead of a single tag, and benchmark FOR+bit-pack vs each alone.
    *(src: Vortex cascading compression — https://vortex.dev/ · FastLanes codec chains, VLDB'23
    — https://www.vldb.org/pvldb/vol16/p2132-afroozeh.pdf)*
  - [ ] **Evaluate Vortex as a columnar interchange + benchmark reference (Phase 1/4):** Vortex
    (Rust, Arrow-compatible, BtrBlocks-based cascading compression, ALP/FastLanes/FSST
    encodings) reports ~100–200× faster random access and 2–10× faster scans than Parquet+zstd
    at similar ratio, with a GPU-decompression roadmap — directly on DSP's random-access +
    GPU-decode + interpolation-native wedge. Assess (a) `vortex-*` crates as an *interchange*
    target beside `dsp-arrow`/Parquet (out-of-core, per the vendor-neutral boundary), and (b)
    Vortex as a `dsp-bench` external-format baseline for the storage/compressed-query workloads.
    *(src: Vortex at Spice.ai, 2025 —
    https://spice.ai/blog/vortex-at-spice-ai-the-columnar-format-for-data-intensive-workloads
    · https://vortex.dev/)*
- [ ] **6.2 Model-based compression** (leverages DSP's spline DNA, NeaTS-like) — piecewise
  linear / spline / polynomial / nonlinear approximation with bounded residuals;
  lossless-residual option; lossy with max-error guarantee; extrema-preserving mode
- [ ] **6.3 Late/compressed-domain execution** — min/max/count from metadata; predicate
  eval before decompression; interpolate directly from model segments; event
  detection over compressed summaries; CPU filter before GPU transfer
- [ ] **6.4 Quality benchmarks** — bytes/point, compress/decompress throughput, random-
  access + range latency, interpolation-after-compression, RMSE/MAE/max-error/bias,
  extrema preservation, event-detection stability, forecasting impact

### Phase 7 — Online perf, durability, correctness · *High for beta*

**Acceptance:** a 24-hour run sustains continuous ingest + concurrent interpolation +
range scans + compression + compaction + late arrivals + simulated failures + restart/
recovery, with bounded p99 and no loss beyond the declared durability mode.

- [ ] **7.1 Online ingest** — scheduled polling daemon, runtime source registration, retry
  buffer, backpressure, idempotency ledger, at-least-once, dedup/upsert, late-arrival
  policy *(maps backlog B-poll, B-retry, B-register, B-dedup)*
- [ ] **7.2 WAL & crash consistency** — WAL design, segment-seal protocol, atomic catalog
  updates, recovery, partial-write handling, fsync policy, durability modes
- [ ] **7.3 Corruption detection** — segment/page checksums *(CRC-32 shipped in the
  `.dspseg` frame)*, catalog checks, startup verification, repair tooling
- [ ] **7.4 Backup/restore** — online backup, PITR if feasible, verification, drills,
  documented RPO/RTO
- [ ] **7.5 Compaction** — scheduling, query consistency during compaction, resource
  limits, metrics, cancellation, priority
- [ ] **7.6 Quotas/limits** — tenant/disk/memory/request-size/query-timeout/GPU-memory

### Phase 8 — Commercial hardening · *Required for paid beta*

**Acceptance:** a design partner can deploy DSP, ingest, run DSP-Bench, inspect
metrics, recover from a restart, and file useful support tickets.

- [ ] Security — TLS, API keys/token auth, basic RBAC, service accounts, secrets, encryption at rest, audit logs, vuln process
- [ ] Compliance readiness — SOC 2, HIPAA (where targeted), GDPR delete/export, retention, tenant isolation
- [ ] Packaging — static binaries, Docker, Compose, Helm (later), systemd, config schema, migration/upgrade/rollback
- [ ] Observability — Prometheus, Grafana, OTel, structured logs, bench dashboard, query profiles
- [ ] SDKs — Rust → Python → TypeScript → R/Arrow
- [ ] Licensing/legal — open-core vs commercial, comparative-benchmark terms, kdb+ restrictions, dependency + codec licenses, customer-data handling, trademark use

### Phase 9 — Analytics premium · *Medium, after benchmark foundation*

Keep the pattern/event/signal roadmap but don't let it block the benchmark-first
engine. Future ML/AI (self-supervised event prediction, causal anomaly detection,
temporal point processes, forecasting export, embedding search over shape summaries)
are **future integrations, not the first identity** — do not prematurely rebrand as a
vector DB. *(Maps backlog Themes 5, 6, 8.)*

- [ ] Sliding windows → overlapping windows → event-centered windows
- [ ] Resample-before-compare
- [ ] Pattern dedup → occurrence-distance constraints
- [ ] Correlation / signal benchmarking
- [ ] Per-stage audit trail
- [ ] *(Future)* ML/AI — self-supervised event prediction, causal anomaly detection, temporal point processes, forecasting export, embedding search over shape summaries

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

Concrete items mined from the 15 archived repos, re-mapped to the phases above.
A checked box = shipped; an unchecked box carries its residual status inline
(`partial`/`absent`/`verify`).

- [ ] **B-tags** *(Phase 2/4 · absent)* — Per-measurement tags/labels (incl. `interpolated=true`/provenance); `measurement.rs` has none. *(src: `DSM-Database`, `DSM-Measurement`)*
- [ ] **B-conn** *(Phase 2/7 · absent)* — Vendor-neutral connector trait + registry (core). *(src: `dsm-source`, `dsm-asset`)*
- [ ] **B-ilp** *(Phase 2 · partial)* — ILP ingest shipped (parser + endpoints); the ILP/Influx **interop** connector crate (not a storage swap) is still to do. *(src: `dsm-influxdb`, `dsm-batch`)*
- [ ] **B-rest** *(Phase 2 · partial)* — REST facade + declarative query-params + pagination shipped (`offset`/`limit`/`take`/`page` + `total`/`count` **and now an opaque `next_cursor`/`?cursor=` forward-iteration token** on `…/points` and `…/value-points`); the `interpolation` query-param alias shipped. Residue: cursor tokens are position-over-read-order (stable while the window is unchanged), not a row-identity keyset — a keyset cursor resilient to concurrent inserts is the next refinement. *(src: `DSM-Database`)*
- [ ] **B-poll / B-retry / B-register** *(Phase 7 · absent)* — Scheduled polling daemon (per-source interval) + at-least-once retry buffer + runtime source registration. *(src: `DSM-Input-Module`)*
- [x] **B-interp** *(Phase 5)* — Interpolate-on-read, single-instant lookup, out-of-range extrapolation. *(src: `splimes`/`database`)*
- [ ] **B-analysis** *(Phase 4/9 · verify)* — Per-point analysis model (signed neighbor distance, slope-segmented trends, max-normalized relative vectors); verify `Trend`/`Relative`/`MeasurementVector`/`Analysis` wired end-to-end. *(src: `dataset_management`, `DSM-Measurement`)*
- [ ] **B-object** *(Phase 4 · absent)* — Schemaless object/annotation store beside numeric aspects. *(src: `DSM-Database`)*
- [ ] **B-windows** *(Phase 9 · absent/partial)* — Sliding/overlapping + event-centered windows; multi-resolution horizon fan-out; min-density validation; config-driven batching policy. *(src: `dsm-batch`, `DSM-Batcher`)*
- [ ] **B-dedup** *(Phase 7 · partial)* — Event-UUID idempotency ledger + delete-after-success queue. *(src: `dsm-batch`, `DSM-Batcher`)*
- [ ] **B-sim** *(Phase 9 · partial)* — Full variability-metric matrix (`static`/`absolute_static`/`percentage`/`absolute_percentage` × `Max`/`Avg`/`Sum`); fixed-point dictionary dedup; occurrence-distance constraint; resample-before-compare (wire to `splimes`). *(src: `DSM-Pattern`)*
- [ ] **B-precision** *(Phase 4/6 · partial)* — `BigDecimal` math audit (no float drift in pattern coords); improved `simplify` (local-extrema). *(src: `DSM-Batch-v2`)*
- [ ] **B-resilience** *(Phase 4/7 · partial)* — Write retry (backoff+jitter) + size-tiered write selector; interpolation window extension (±N steps). *(src: `legacy/database`)*
- [ ] **B-conncache** *(Phase 4/7 · partial)* — Complete the cached-connection migration (TTL/LRU connection cache + `begin_concurrent` shipped for core input/query ops; batch/pattern/correlation ops still on `begin_concurrent_direct`); add pool metrics/health checks; benchmark before/after; tune cache size + TTL. *(src: the retired `database/CACHED_CONNECTIONS.md` note — verify current state in `database/src/types/cache.rs`)*
- [ ] **B-msgpack** *(Phase 6 · absent)* — MessagePack for queued artifacts (vs JSON). *(src: `dsm-batch`)*
- [ ] **B-snapshot** *(Phase 3/9 · absent)* — Pipeline state-snapshot audit trail (before/after JSON per stage). *(src: `DSM-Log`, `DSM-Batch-v2`)*

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

## Control-plane engine: Turso/libSQL 0.6 adoption

DSP pins `turso = "0.6"`. Per hard-constraint #3, Turso/libSQL is the **control
plane** only — the features below are evaluated **only** for control-plane use; none
of them turn Turso into the measurement backend.

- [x] Migration 0.4 → 0.6 — Turso 0.6 rejects `AUTOINCREMENT` under
  `PRAGMA journal_mode=experimental_mvcc` at parse time; all control-plane schemas
  migrated to plain `INTEGER PRIMARY KEY` (still a rowid alias). MVCC concurrent
  writes (`BEGIN CONCURRENT`) remain the basis of the write path.
- [ ] Lean on production MVCC concurrent writes for concurrent ingest + catalog updates under load *(Phase 7)*
- [ ] Encryption at rest (AEAD pager) for catalog/metadata/pipeline-state DBs *(Phase 8)*
- [ ] `VACUUM INTO 'file'` for online, consistent control-plane backup/snapshot *(Phase 7.4)*
- [ ] Triggers (`BEFORE/AFTER/INSTEAD OF` + `WHEN`) — enforce catalog invariants, emit audit-log rows on metadata mutations *(Phase 7/8)*
- [ ] `Statement::n_change()` affected-row accounting for idempotent batch ingest + instrumentation spans *(Phase 3/7)*
- [ ] Dynamic auth tokens as closures — hosted/remote control-plane credential rotation *(Phase 8)*
- [ ] `UPDATE … FROM`, aggregate `FILTER`, `INDEXED BY`, `NULLS FIRST/LAST` — simplify catalog/metadata queries *(Phase 2/4)*
- [ ] Evaluate (don't rush): native vector search over pattern/shape **summaries** only *(Phase 9 — never over raw measurements)*; CDC/sync engine for online ingest/replication *(assess once 7.2 is solid)*; custom I/O (`with_io_impl`) / generated columns *(only on concrete need)*

**Explicitly defer / avoid:** multi-process WAL access (`?experimental=
multiprocess_wal`) — **incompatible with `BEGIN CONCURRENT`**, which the write path
depends on; treating any Turso table as the measurement store (violates
hard-constraint #3).

---

## Six-month execution plan

- [ ] **Month 1 — Benchmark truth:** external-engine adapters (DSP + DuckDB minimum, then ClickHouse/InfluxDB 3/QuestDB/TimescaleDB); first cross-engine report (no aggressive claims); methodology + anti-Goodhart policy + hardware-reporting template + correctness rules published
- [ ] **Month 2 — Public API & observability:** OTel; Parquet bench output; cold/warm separation; GPU timing breakdown *(server, REST, ILP, Prometheus, p50/p95/p99 shipped)*
- [ ] **Months 3–4 — Hot-path optimization:** batch-ingest rewrite; minimized `BigDecimal` hot-loop use; typed-array interpolation streaming; nightly benchmark-regression CI. *Goal: establish whether storage or conversion is the dominant bottleneck.*
- [ ] **Months 4–5 — GPU flagship:** command batching; true async handles; CPU/GPU overlap; hardware auto-tuning; end-to-end interpolation benchmarks; CPU-only vs GPU cost/performance report; portability/determinism matrix. *Publish only if strong and reproducible.*
- [ ] **Month 5 — Compression v2 prototype:** scaled-int compression; f64 codec prototype; random-access blocks; model-based prototype; accuracy benchmarks
- [ ] **Month 6 — Commercial beta:** Docker image; API keys/TLS; backup/restore MVP; Python SDK; Grafana dashboard; benchmark report; customer-runnable DSP-Bench; design-partner onboarding docs. *Goal: a "DSP Performance Preview" for 3–5 partners.*

---

## Benchmark report requirements

Every public report must include: **hardware** (CPU model/cores/threads, RAM, disk +
filesystem, GPU + memory, PCIe gen, OS/kernel, drivers, cloud instance + hourly
cost); **systems** (every system's version/git SHA/config); **datasets** (regular +
irregular IoT, high-frequency finance, scientific sensor, long single series, many
short series, sparse/missing, block missingness, compression corpus, anonymized
customer data where allowed); **workloads** (ingest, online ingest/query, raw range
fetch, point lookup, aggregation, downsampling, interpolation, gap filling,
compression, compressed query, analytics pipeline); **metrics** (points/sec,
bytes/sec, p50/p95/p99/max latency, storage bytes/point, compression ratio + speed,
random-access latency, GPU upload/kernel/readback, memory, CPU + GPU utilization,
freshness lag, cost estimate, correctness/error metrics); and a **required honesty
section** (where DSP wins, where it loses, workloads not tested, known limitations,
config caveats, reproduction instructions).

---

## Research & business notes

**Benchmarking research:** *TSM-Bench* (PVLDB'23) and *SEER* (PVLDB'24) →
macrobenchmarks with realistic data, concurrency, dashboards, reproducibility;
DSP-Bench should resemble SEER. *SciTS* → growth curves (10M→100M→1B). *TSBS* →
implement for compatibility (via ILP), not as the primary proof.

**Competitor architecture (don't benchmark stale mental models):** *InfluxDB 3*
(FDAP stack) — compete where it's less specialized (interpolation-heavy irregular,
precision, GPU resampling, model compression), not on broad scans. *TimescaleDB
Hypercore* (hybrid row/columnar) → DSP needs its own hot/cold story. *ClickHouse*
(vectorized OLAP) → don't fight on generic OLAP first. *QuestDB* (ILP, `SAMPLE BY`,
ASOF) → strong finance/high-ingest comparison. *Apache IoTDB/TsFile* → validates a
time-series-native columnar file layout. *DuckDB* → excellent CPU-only baseline.
*kdb+* → optional; **legal review before publishing comparisons.**

**Compression research:** *Chimp* (PVLDB'22), *Elf* (PVLDB'23), *ALP* (SIGMOD'24,
vectorized), *NeaTS* (ICDE'25, nonlinear + random access — aligns with DSP's spline
DNA). Judge compression by *downstream task impact*, not only bytes.

**GPU databases:** the bottleneck is end-to-end data movement (PCIe, capacity,
orchestration), not kernel speed — *Vortex* (PVLDB'24/25), *Scaling GPU DBs Beyond
GPU Memory* (PVLDB'25). *GPUDirect/BaM* = CUDA-centric long-term research, not
`wgpu`-portable → defer.

**Irregular/missing data:** *TSI-Bench* etc. → missingness pattern matters; MCAR can
mislead; block/subsequence missingness is harder; linear interpolation is a strong
baseline → benchmark interpolation **quality** (extrema preservation,
event-detection stability) as well as speed.

**Target personas (feel interpolation/resampling pain):** quant finance / market
data; industrial IoT / SCADA; energy / grid monitoring; scientific instrumentation;
robotics / autonomous telemetry; healthcare / wearables (if compliance scope is
manageable). **Design-partner criteria:** irregular data; large historical ranges;
production interpolation/gap filling; pain with current TSDB/app-layer workflow;
measurable latency/cost targets; willing to run DSP-Bench and share anonymized
results. *Avoid partners who only need generic dashboards.*

**Pricing hypotheses:** open-source core; paid enterprise server; paid GPU
acceleration; paid advanced compression; paid security/compliance; paid benchmark
consulting; managed DSP-Bench reports; support subscription. **Validate willingness
to pay before building expensive distributed features.** Key question: *is
interpolation performance a budget-owning pain, or merely an engineering annoyance?*

**Legal (before publishing comparative results):**

- [ ] Review each competitor's benchmark-publication terms (kdb+ especially); disclose configs; include reproduction instructions; check dependency + **compression-codec patents/licenses**; document trademark usage; clarify whether benchmark data may be redistributed

---

## Immediate next actions

- [x] Create `dsp-bench` as a first-class workspace member
- [x] Define the first benchmark profile: `interpolation-heavy-irregular`
- [x] DSP adapter + portable baselines (linear class-C, forward-fill class-B) + accuracy scoring + shape-selectable ground truth
- [ ] Add the **DuckDB** adapter — the first external-engine (real database) baseline
- [ ] Add ClickHouse, InfluxDB 3, QuestDB, TimescaleDB adapters
- [x] Implement InfluxDB Line Protocol ingest (shared `dsp-line-protocol` crate; bench + server wired end-to-end)
- [ ] Add end-to-end timing spans *(harness-level spans shipped; per-pipeline-stage spans = Phase 3 tracing item)*
- [x] **Cross-segment out-of-order merge (Phase 4.6):** shipped — overlap detection
  (`SegmentIndex::overlapping_count`, surfaced in per-aspect + store-wide stats),
  the `merge_newer_wins` kernel, `SegmentStore::reconcile_overlaps`/`reconcile_all_overlaps`
  (+ `SegmentIndexStore::delete`), the `?overlaps=true` reconcile endpoints, and the
  `DSP_RECONCILE_OVERLAPS` background daemon sweep
- [x] **Hot/cold split in the background reconcile (Phase 4.6):** shipped —
  `reconcile_aspect_hot_cold`/`reconcile_all_hot_cold`, `?hot_cold=true` endpoints,
  `DSP_RECONCILE_HOT_COLD` daemon mode
- [x] **Split-not-rewrite optimization (Phase 4.6):** shipped — `split_segment`
  primitive, `reconcile_overlaps_with_policy` (consumes `SplitPolicy::decide`; default
  `reconcile_overlaps` keeps the 50 MiB floor), the `?split_min_bytes=` query param on
  the per-aspect and store-wide reconcile endpoints, and the `DSP_RECONCILE_SPLIT_MIN_BYTES`
  daemon env; plus the **squash** half — `squash_aspect`/`squash_aspect_if_exceeds`/
  `squash_all_over_threshold`, `POST …/{aspect}/squash?max_segments=`, and the
  `DSP_RECONCILE_MAX_SPLITS` daemon env *(src: https://questdb.com/docs/concepts/partitions/)*
- [x] **Per-stage tracing child spans (Phase 3):** shipped — `interpolate.parse`/
  `interpolate.compute`/`interpolate.serialize` under `interpolate.engine`,
  `downsample.parse` beside `downsample.reduce`, `storage.ingest.parse`/`storage.ingest.seal`
  on the write path, and a per-request root span (`request{method,path,request_id}` +
  `x-request-id`) they all nest under
- [x] **OTLP trace export (Phase 3):** shipped — env-gated OTLP/gRPC `SdkTracerProvider`
  (batch exporter + `dsp-server` resource) + `tracing_opentelemetry` layer beside the
  `fmt` subscriber; deps `opentelemetry`/`opentelemetry_sdk`/`opentelemetry-otlp` 0.32 +
  `tracing-opentelemetry` 0.33 (build protoc-free). Runtime-verified boot both with and
  without `OTEL_EXPORTER_OTLP_ENDPOINT`.
- [x] **OTLP collector verification: shipped.** Local Jaeger collector (docker,
  auto-restart) + `scripts/verify-otlp.sh` — asserts `request` root spans with nested
  storage stage spans land via the Jaeger query API; re-runnable headlessly.
- [x] **Cursor paging (Phase 2 B-rest):** shipped — opaque `next_cursor`/`?cursor=`
  forward-iteration token on `…/points` + `…/value-points`
- [x] **Gorilla-codec adopt-or-drop (Phase 6.1): ADOPTED + realized.** Benchmarked on a
  scattered-single-jitter corpus (~28% below the best shipped codec in its ±2048 regime,
  honest loss past it); realized as a lossless codec, wired into the `.dspseg` block +
  `best_estimated_bytes`/`best_encoding_name`, and surfaced in dsp-bench. RLE realized on
  disk in the same arc.
- [x] **Scaled-int value bit-pack codec (Phase 4/6): shipped + realized on disk.** The
  value block carries a `VAL_CODEC_VARINT`/`VAL_CODEC_BITPACK` selector; a `ScaledI64`
  column bit-packs when smaller (`best_value_codec`), surfaced via
  `StorageEstimate.realized_value_bytes`/`value_codec` (schema v8). Measured 37.4% below
  varint on a 480-pt 2-decimal ramp; runtime `bytes_per_point=2.13` via `/…/stats`.
- [x] **Per-block adaptive (dynamic) bit-pack timestamp codec (Phase 6.1): shipped.** The
  fifth `.dspseg` timestamp codec (`TS_CODEC_BLOCKED`), strict winner on mixed-magnitude
  streams (184 B vs 384–961 B for the other codecs on the eval corpus).
- [x] **`storage.ingest.parquet` stage span (Phase 3): shipped.** Closes the
  Arrow-decode-on-ingest tracing gap; runtime-verified nesting under the request root span
  (`byte_len`, `require_sorted`, `format="parquet"`).
- [x] **FOR (frame-of-reference) before bit-packing (Phase 6.1): prototyped + benchmarked
  (advisory).** `for_bitpack_*` beside the blocked codec (per-block reference, unsigned
  residual packed to the block *range*): `for_estimated_bytes` (timestamp),
  `for_value_bytes` (value), surfaced in the bench as
  `StorageEstimate.advisory_for_value_bytes` (schema v9). Measured 82% below per-block
  bit-pack on clustered timestamp dods, 88% below on value mantissas clustered near 1e9.
- [x] **Per-block adaptive (blocked) VALUE codec (Phase 6.1): shipped + realized on disk.**
  `VAL_CODEC_BLOCKED` selected via `best_value_codec` (`scaled_blocked`), strict winner on
  a mixed-magnitude / zero-straddling mantissa column; regular columns byte-for-byte
  unchanged. Round-trip- + framed-segment-verified.
- [ ] **Next slice — FOR adopt-or-drop (Phase 6.1, OWNER-GATED):** FOR is advisory; adopting
  it (wiring `scaled_for`/a timestamp FOR codec into the selectors + `.dspseg`) flips the
  realized codec / bytes-per-point of most non-negative columns (FOR packs unsigned, beating
  zig-zag bit-pack broadly) — decide together with the headline-bytes/point flip below. Value
  column is the strong adopt candidate (clustered-high-base sensor regime); timestamps rarely
  win (small dods) → recommend keep-advisory/drop for timestamps.
- [ ] **Next slice — FastLanes transposed bit-unpack (Phase 6.1, decode-speed):** prototype a
  1024-value transposed layout for the realized `ScaledI64` bit-pack so decode auto-vectorizes
  (>100 B ints/sec); bytes/point unchanged, a decode-latency win that also opens a GPU-unpack
  path. *(src: FastLanes, VLDB'23 — https://www.vldb.org/pvldb/vol16/p2132-afroozeh.pdf)*
- [ ] **Next slice — evaluate Vortex as interchange + bench baseline (Phase 1/4):** Rust,
  Arrow-compatible, cascading ALP/FastLanes/FSST codecs, ~100–200× faster random access than
  Parquet+zstd, GPU-decode roadmap — on DSP's random-access + GPU + interpolation wedge.
  *(src: https://vortex.dev/ ·
  https://spice.ai/blog/vortex-at-spice-ai-the-columnar-format-for-data-intensive-workloads)*
- [ ] **Owner decision — flip the headline bytes/point to the realized figure (Phase 4/6):**
  the value block now realizes the smaller codec on disk, but `estimated_bytes` /
  `bytes_per_point` / the bench HTML `val B/pt` still report the naive `len*8`; deciding to
  flip is a semantic change across ~37 callers / ~33 assertions (owner's call).
- [ ] **Discovered — dsp-bench parallel-test OOM (needs repro):** the full `cargo test -p
  dsp-bench` intermittently aborts with `memory allocation of ~13 GB failed` under
  concurrent memory pressure; `--lib` alone and `--test-threads=1` pass deterministically
  (89+20/0), so it is environmental (heavy parallel build+test load), not a logic bug in
  the tree. Investigate whether a specific bench/integration test transiently over-allocates
  when interleaved; cap its parallelism or size if so.
- [x] Add p50/p95/p99 + confidence-interval reporting
- [x] Add physical value types (`F64`, `ScaledI64`, `BigDecimalText` + three more)
- [x] Prototype columnar segment reads for one aspect type (`database::SegmentStore`)
- [ ] Publish a methodology document **before** any performance claim

> The key commercial move is not adding features — it is making DSP's performance
> claims measurable, reproducible, and valuable to a specific buyer. If DSP can
> credibly show it is faster, cheaper, or more accurate for large-scale interpolation
> and compression-aware irregular time-series analysis than general-purpose TSDBs, it
> has a clear path to a commercial product.
