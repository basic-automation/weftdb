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
  Decimal128/scaled-int codecs; **block-level random access** *(shipped —
  `blocked`/`for_bitpack_decode_range` + `dspseg::read_value_at`)*. *(Check codec
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
  - [x] **FOR adopt-or-drop — DECIDED (owner sign-off): ADOPTED for the value column,
    timestamps stay advisory.** `VAL_CODEC_FOR` is the fourth realized `.dspseg` value
    codec, folded into `best_serialized_bytes`/`best_value_codec` (`scaled_for`, chosen
    only when strictly smallest; ties keep the simpler codec); a non-`ScaledI64` payload
    is rejected (`value_codec_for_type`). Because FOR packs the residual *unsigned* it
    beats zig-zag bit-packing on *any* all-non-negative column — an accepted, genuine
    win, decided together with the headline bytes/point flip below so the metric broke
    once, not twice. The bitpack/blocked-regime tests moved to zero-straddling fixtures
    (their genuine regime). Timestamp FOR stays advisory-only (`for_estimated_bytes`):
    dods are small/near-zero so FOR rarely wins there; the advisory number remains as
    the tripwire if a real corpus ever contradicts that. Round-trip- +
    framed-segment-verified.
  - [x] **Sprintz-style FIRE predictor + zero-RLE (advisory):** shipped as advisory
    estimates. `timestamp::fire_residuals`/`fire_reconstruct`/`fire_estimated_bytes` — FIRE
    (Fast Integer REgression) generalizes delta-of-delta with a learned fixed-point
    coefficient (sign-sign LMS, `FIRE_SHIFT`=8) adapted online, verified-lossless by the
    deterministic wrapping-i64 encoder/decoder mirror; the estimate takes the best of
    {varint, bit-pack, blocked bit-pack, **RLE**} over the residual tail (the zero/run-length
    half). Surfaced in the bench as `StorageEstimate.advisory_fire_timestamp_bytes` (schema
    v14) so the FIRE-vs-dod question is answered on the *real* timestamp corpus. Measured
    54.8% below dod on a constructed geometric-velocity stream. *(src: Sprintz, ACM TODS'18 —
    https://arxiv.org/abs/1808.02515)*
  - [ ] **FIRE adopt-or-drop — gate on a REAL-corpus win, do not assume one.** Honest
    upstream finding: the Sprintz authors themselves and a fresh comparative study report
    FIRE's improvement over plain delta is *marginal* on real data (the study omits FIRE for
    that reason). So the constructed 54.8% win is not evidence to adopt — use the shipped
    `advisory_fire_timestamp_bytes` to check whether FIRE actually beats dod on DSP's real
    irregular timestamp corpora before wiring it into any on-disk selector; drop it if it does
    not. *(src: "Lossless Compression of Time Series Data: A Comparative Study", 2025 —
    https://arxiv.org/html/2510.07015v1 · Sprintz §FIRE — https://arxiv.org/abs/1808.02515)*
  - [ ] **Evaluate Pcodec as an integer/f64 codec + bench baseline:** the 2025 comparative
    study finds **Sprintz and Pcodec** give the best ratio/throughput trade-off for integer
    time-series (Sprintz at Snappy/LZ4 speeds). Pcodec is Rust, columnar, and directly on
    DSP's scaled-int + f64 hot path — assess it as a codec and as a `dsp-bench` external-format
    baseline beside Vortex. *(src: https://arxiv.org/html/2510.07015v1)*
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
  - [x] **Value-column realized-bytes accuracy — headline FLIPPED (owner sign-off,
    bench schema v10).** `Segment::value_bytes()`/`total_bytes()`/`bytes_per_point()`,
    `Page::total_bytes()`, and `StorageEstimate.bytes_per_point`/`total_bytes_per_point`
    (and the bench HTML `val B/pt`) now report the **realized** figure — the codec each
    column actually writes. The naive fixed-width figure is retained as the comparison
    baseline (`Segment::logical_value_bytes()` / `StorageEstimate.estimated_value_bytes`),
    so `realized / logical` reads as the value column's compression ratio. Pre-v10
    bytes/point artifacts are NOT comparable (the old headline reported the uncompressed
    size and oversold storage cost); the now-redundant `advisory_for_value_bytes` field
    was dropped in the same bump.
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
  - [x] **f64 value-column codec — Gorilla + Chimp XOR codecs + best-of selector (advisory).**
    The `F64` value column had no compression (raw 8 B/value IEEE pattern). Shipped in
    `dsp-physical-type::floatcodec`: `xor_f64_*` (Gorilla — XOR vs the immediate predecessor,
    store only the meaningful bits between the leading/trailing zeros) and `chimp_f64_*`
    (single-predecessor Chimp — 2-bit flags, a 3-bit leading-zero class, trailing-zero trim),
    both bit-exact for every f64 (NaN/±inf/subnormal/signed-zero, XORs `to_bits`); a
    `best_f64_codec` selector over {raw, gorilla, chimp} (never worse than raw); the
    `ColumnEncoding` advisory methods (`gorilla_f64_bytes`/`best_f64_bytes`/`best_f64_codec`);
    and the `dsp-bench` `StorageEstimate.advisory_best_f64_bytes`/`advisory_best_f64_codec`
    (schema v12) surfacing the best f64 saving on a lossy-tolerance run (measured Gorilla
    ~45% below raw on a stable-exponent column near 1000). **Advisory only** — no f64 codec is
    on disk yet. Honest finding: single-predecessor Chimp is *not* a universal win over
    Gorilla (Gorilla's reuse-window path re-uses a repeated long-trailing window that Chimp's
    trim path must re-header, and Chimp's leading-class rounding writes extra bits) — the
    decisive win needs the 128-value window below. *(src: Gorilla, VLDB'15
    https://www.vldb.org/pvldb/vol8/p1816-teller.pdf · Chimp, VLDB'22
    https://www.vldb.org/pvldb/vol15/p3058-liakos.pdf)*
  - [x] **Faithful Chimp128 (128-value reference window) — shipped.**
    `floatcodec::chimp128_f64_encode`/`decode`/`bytes`: a 128-value ring + a lookup table keyed
    on the low 14 bits → the most-recent ring index with that pattern (reference found in O(1),
    the trailing-zero threshold guaranteed by the hash), with the 2-bit-flag/4-way serialization
    faithful (flag packed into the payload's high bits). Bit-exact for every f64; folded into
    `best_f64_codec` (chosen when strictly smallest) so it flows to the bench
    `advisory_best_f64_codec`. Measured 82.8% below Gorilla on a period-4 revisiting signal (the
    regime the single-predecessor codecs cannot reach). *(src: Chimp128 —
    https://www.vldb.org/pvldb/vol15/p3058-liakos.pdf · duckdb notes —
    https://github.com/duckdb/duckdb/pull/4878)*
  - [x] **Elf (erasing-based) f64 codec — shipped (conservative advisory variant).**
    `floatcodec::elf_f64_encode`/`decode`/`bytes`: a column-shared decimal grid (`alpha` = max
    fractional digits, one header byte) + a greedy erase that keeps the most-erased low-bit
    pattern whose `round(·,alpha)` restore reproduces the exact value — verified-lossless by
    construction — then the Chimp128 backend. Measured 6.7% below Chimp128 on distinct 2-decimal
    drift. Residue: the **faithful bit-level closed-form erase**, which upstream reports at **12%
    below Chimp128 / 47% below Chimp** on time-series (my column-grid variant leaves bits on the
    table). *(src: Elf, VLDB'23 — https://www.vldb.org/pvldb/vol16/p1763-li.pdf · adaptive Elf,
    arXiv'23 — https://arxiv.org/pdf/2308.11915)*
  - [ ] **f64-codec adopt: target ALP, not Chimp128/Elf — the state of the art.** Upstream
    evidence (this run's research): **ALP replaced Chimp128 + Patas in DuckDB**, decodes ~2.6
    doubles/CPU-cycle, and beats *both* Chimp128 and Elf on compression ratio **and** speed on
    almost every dataset (Elf/Chimp128 only win where repeated values dominate and precision is
    highly variable — ALP's `ALPrd` sub-scheme is the fallback there). So the f64 adopt-or-drop
    should benchmark **ALP** (with the `ALPrd` outlier path) against the shipped
    Gorilla/Chimp/Chimp128 and the advisory Elf, pick ALP unless a repeated-heavy corpus says
    otherwise, then realize it as a `VAL_CODEC_*` and flip `best_f64_codec`/the bench headline —
    **owner sign-off, as with FOR** (it is a headline change). Judge by bytes/point AND decode
    throughput (FCBench / VLDB'25 methodology). *(src: ALP, SIGMOD'24 —
    https://dl.acm.org/doi/10.1145/3626717 · DuckDB ALP — https://duckdb.org/library/alp/ ·
    comprehensive eval, VLDB'25 — https://www.vldb.org/pvldb/vol18/p4396-hishida.pdf · FCBench —
    https://arxiv.org/pdf/2312.10301)*
  - [ ] **GPU-decode ALP — on DSP's GPU + compression wedge:** a Nov-2025 paper presents a
    high-throughput **GPU** framework for adaptive lossless f64 compression (ALP-style). Once an
    ALP-class codec is on disk, a GPU-unpack path pairs directly with DSP's GPU interpolation
    flagship (decompress-on-device, no host round-trip). Evaluate after the CPU ALP adopt lands.
    *(src: "A High-Throughput GPU Framework for Adaptive Lossless Compression of Floating-Point
    Data", arXiv 2511.04140 — https://arxiv.org/pdf/2511.04140)*
  - [x] **FastLanes "Unified Transposed Layout" for the bit-pack codecs (decode-speed
    slice): shipped (prototype).** `TRANSPOSE_TILE`/`transpose_bitpack_bytes`/`_encode`/`_decode`
    — a per-tile bit-plane-major layout whose decoder reads `u64` words and walks only the *set*
    bits (`w &= w-1`), so the empty high bit-planes of a small-magnitude stream are skipped
    wholesale (where the scalar per-value loop pays every bit of every value). Byte footprint is a
    permutation of the linear per-block layout (identical on aligned tiles). Measured (release,
    `benches/bitunpack.rs`, 1 Mi small-magnitude stream): **~238 Melem/s vs ~41.6 Melem/s** for the
    linear per-block decode at the same footprint — **~5.7× decode speedup**, bytes/point unchanged.
    Residue: realize it as the stored blocked layout behind a format-version bump + reader dispatch,
    and measure the *end-to-end* read win (decode is often bandwidth-bound —
    https://arxiv.org/pdf/2606.22423). *(src: FastLanes Compression Layout, VLDB'23 —
    https://www.vldb.org/pvldb/vol16/p2132-afroozeh.pdf)*
  - [x] **Cascading (recursive) codec composition — delta→best-packer chain: advisory + on disk.**
    DSP's other value codecs are single-level (one of varint/bit-pack/blocked/FOR); this is the
    first *recursive* codec. Shipped as the advisory `ColumnEncoding::delta_cascade_bytes`/
    `delta_cascade_plan` (delta-transform the mantissas, then best-of {varint, bit-pack, blocked,
    FOR, RLE} over the differences — a monotone-trend column that defeats every single-level codec
    drops 97.4%, the trend collapsing to a constant RLE run) **and** the on-disk codec-chain
    descriptor `VAL_CODEC_DELTA_CASCADE` in the `.dspseg` value block (anchor + inner-codec
    descriptor + inner-coded deltas), written by `write_value_column_cascading` and read by the
    ordinary `read_value_column`. **Opt-in** (the cascade beats FOR on FOR's own fixtures → a broad
    realized-bytes change; default-adoption is owner-gated). *(src: Vortex cascading compression —
    https://vortex.dev/ · cascading-with-BtrBlocks —
    https://spiraldb.com/post/cascading-compression-with-btrblocks · FastLanes codec chains,
    VLDB'23 — https://www.vldb.org/pvldb/vol16/p2132-afroozeh.pdf)*
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
- [x] **FOR adopt-or-drop (Phase 6.1): DECIDED — ADOPTED for the value column (owner
  sign-off).** `VAL_CODEC_FOR` realized as the fourth `.dspseg` value codec
  (`scaled_for`, strict-win selection); timestamps stay advisory (small dods, FOR
  rarely wins). Decided together with the headline flip below — one metric break.
- [x] **FastLanes transposed bit-unpack (Phase 6.1, decode-speed): shipped (prototype).**
  `TRANSPOSE_TILE`/`transpose_bitpack_bytes`/`_encode`/`_decode` — a per-tile bit-plane-major
  layout whose decoder reads `u64` words and walks only the *set* bits (`w &= w-1`), so the empty
  high bit-planes of a small-magnitude stream are skipped wholesale. Byte footprint identical to
  the linear per-block layout (a bit permutation). Measured (release, criterion `benches/bitunpack.rs`):
  transposed **~238 Melem/s (4.40 ms)** vs linear per-block **~41.6 Melem/s (25.2 ms)** at the same
  footprint — **~5.7× decode speedup**, bytes/point unchanged. *(src: FastLanes, VLDB'23 —
  https://www.vldb.org/pvldb/vol16/p2132-afroozeh.pdf)*
- [ ] **Next slice — realize the transposed layout on disk (Phase 6.1, decode-speed residue):**
  the transposed decode is a proven ~5.7× win but is a prototype on no read path. Realize it as
  the stored layout for the blocked value/timestamp codec (identical bytes on aligned tiles) behind
  a segment-format-version bump with reader dispatch (old version → linear decode, new → transposed),
  and benchmark the end-to-end read-decode win. Note the **decode-throughput-is-often-bandwidth-bound**
  caveat before claiming an end-to-end win — measure, don't assume. *(src: FastLanes, VLDB'23 —
  https://www.vldb.org/pvldb/vol16/p2132-afroozeh.pdf · "When Is a Columnar Scan Bandwidth-Bound? A
  Decode-Throughput Law", 2026 — https://arxiv.org/pdf/2606.22423)*
- [ ] **Next slice — evaluate Vortex as interchange + bench baseline (Phase 1/4):** Rust,
  Arrow-compatible, cascading ALP/FastLanes/FSST codecs, **~100× faster random access + 10–25×
  faster decode than Parquet+zstd at ~same ratio (TPC-H SF10, 38% smaller)**, GPU-SIMT decode by
  design — directly on DSP's random-access + GPU + interpolation wedge, and Vortex now an LF AI &
  Data incubation project (stable enough to depend on). Vortex's own guidance — cascading wins on
  "auto-incrementing IDs, sensor readings with bounded variation" — independently validates DSP's
  just-shipped delta-cascade value codec. *(src: https://vortex.dev/ ·
  https://spice.ai/learn/vortex · cascading-with-BtrBlocks —
  https://spiraldb.com/post/cascading-compression-with-btrblocks)*
- [ ] **Evaluate the FastLanes *File Format* (not just the layout) as a bench baseline (Phase 1):**
  the 2025 FastLanes file-format paper extends the transposed layout to a full format; assess it
  beside Vortex/Parquet as a `dsp-bench` external-format baseline for the storage/compressed-query
  workloads. *(src: "The FastLanes File Format", 2025 —
  https://www.researchgate.net/publication/395278946_The_FastLanes_File_Format)*
- [x] **Headline bytes/point FLIPPED to the realized figure (Phase 4/6, owner sign-off,
  bench schema v10):** `Segment::value_bytes`/`total_bytes`/`bytes_per_point`,
  `Page::total_bytes`, `StorageEstimate.bytes_per_point`/`total_bytes_per_point`, and
  the bench HTML `val B/pt` now report the codec actually written; the naive `len*8`
  figure is retained as `logical_value_bytes`/`estimated_value_bytes` (the compression
  baseline). Pre-v10 bytes/point artifacts are not comparable.
- [x] **dsp-bench parallel-test OOM — ROOT-CAUSED + FIXED (it was a logic bug, not
  environmental).** A full-backtrace capture pinned the ~28 GB allocation to
  `splimes::helpers::generate_target_times::TargetTimesIterator::next_impl`, which sized the
  per-batch `Vec<DateTime<Utc>>` from *free system memory* (`available_memory / point_size /
  2`) instead of from the number of timestamps to produce — a tens-of-GiB speculative
  `Vec::with_capacity` per call. Single-threaded one such allocation succeeds when RAM is
  idle (only ever filled to the tiny real grid, then dropped), which is why `--test-threads=1`
  masked it; under the default parallel harness many interpolation tests call it at once and
  the summed over-allocation aborts the process. Fixed by capping the batch by
  `remaining_points()` (the timestamps left) and `MAX_BATCH_POINTS` (1 << 20); the previously
  reliable crash is gone (`cargo test -p dsp-bench --lib` now passes 92/0 on repeated parallel
  runs). The sibling sizers in `optimizations/mod.rs` + `gpu/mod.rs` were already bounded by
  the real work — no change needed there.
- [x] **f64 value-column codecs (Phase 6.1): Gorilla + Chimp + Chimp128 + Elf-style, best-of
  selector (advisory), bench-surfaced (schema v13).** Faithful **Chimp128** (14-bit-trailing-hash
  128-value reference window) shipped + folded into `best_f64_codec` (82.8% below Gorilla on a
  revisiting signal); **Elf-style** erasing codec shipped as a verified-lossless conservative
  advisory (6.7% below Chimp128 on 2-decimal drift; faithful bit-level erase is the residue). See
  the Phase 6.1 f64-codec block. **Adopt target is ALP** (replaced Chimp128 in DuckDB, beats both
  Chimp128 and Elf on ratio+speed) — the adopt-or-drop + on-disk realization is owner-gated (a
  headline change, as FOR was).
- [x] **Sprintz FIRE forecaster + zero-RLE (Phase 6.1): shipped advisory + bench-surfaced (schema
  v14).** `fire_residuals`/`fire_reconstruct`/`fire_estimated_bytes` (learned-coefficient
  delta-of-delta generalization, verified-lossless) with the RLE zero-run half; surfaced as
  `StorageEstimate.advisory_fire_timestamp_bytes`. 54.8% below dod on a constructed
  geometric-velocity stream — but FIRE is *marginal on real data* (upstream + the 2025 comparative
  study), so adoption is gated on the bench advisory showing a real win.
- [x] **Cascading codec composition (Phase 6.1): advisory + on-disk descriptor shipped.**
  `ColumnEncoding::delta_cascade_bytes`/`delta_cascade_plan` (delta→best inner packer over
  {varint,bit-pack,blocked,FOR,RLE}; 97.4% below the best single-level codec on a monotone-trend
  column) **and** the on-disk `.dspseg` codec-chain descriptor `VAL_CODEC_DELTA_CASCADE` (anchor +
  inner-codec descriptor + inner-coded deltas; `write_value_column_cascading` /
  `best_value_codec_cascading`, decoded by the ordinary `read_value_column`). Surfaced as
  `StorageEstimate.advisory_delta_cascade_value_bytes` (bench schema v15). Kept **opt-in** — the
  cascade beats FOR on FOR's own clustered fixtures, i.e. a broad realized-bytes change of the FOR
  class, so the default selector is unchanged. *(Vortex's own guidance independently confirms the
  regime — cascading wins on auto-incrementing IDs + bounded-variation sensors:
  https://spiraldb.com/post/cascading-compression-with-btrblocks)*
- [ ] **Cascade adopt-or-drop into the DEFAULT selector — owner-gated (headline change, as FOR/ALP):**
  fold the delta cascade into `best_value_codec`/`best_serialized_bytes` so trending value columns
  realize it by default (bench schema bump; pre-adoption bytes/point not comparable on trending
  columns). Broad win — needs owner sign-off, then flip and re-baseline the headline once.
- [x] **Block-level random access (Phase 6.1): shipped.** `blocked_bitpack_decode_range` /
  `for_bitpack_decode_range` decode only the blocks overlapping a `[start, len)` window (skipping
  earlier blocks by their headers), and `dspseg::read_value_at(bytes, index)` reads one value
  straight from a `.dspseg` value block — the per-block codecs take the block-skip fast path, the
  rest fall back to a full decode + index. The point-lookup / late-materialization lever, matching
  Vortex's finer-grained in-segment access. *(src:
  https://spice.ai/learn/vortex)*
- [x] **Streaming single-value point read wired into the segment/API point-read path (Phase 4/6):
  shipped.** `dspseg::read_segment_point(bytes, t)` reads the first present value at `t` from a
  single-block `.dspseg` frame **without materializing the value column**: on a per-block value
  codec (`VAL_CODEC_FOR` / `VAL_CODEC_BLOCKED`) it skips the value block by its self-describing
  framing, decodes only the timestamp block to locate the row (binary-search sorted, linear-scan
  out-of-order), maps the logical row to its dense present-rank, and unpacks the *one* covering
  value block via `read_value_at`; every other codec falls back to `read_segment` + `value_at`
  (equal for every frame). `SegmentStore::read_point` (which powers `GET …/storage/{aspect}/at`)
  now takes this path for single-block segments. Benchmarked (`benches/pointread.rs`, criterion,
  100k-row sorted FOR segment): **222.65 µs vs 13.196 ms full-decode — ~59× faster point lookup,
  identical bytes on disk.**
- [x] **Streaming point read extended to paged segments (Phase 4/6): shipped.**
  `dspseg::read_paged_segment_point(bytes, t)` parses the per-page index (stats + block length)
  and **prunes pages on their indexed min/max ts without decoding a column byte** (on-disk
  intra-segment page skipping), then resolves the surviving page through the shared
  `read_point_from_section` (the block-skip value read applies within the page too), matching
  `PagedSegment::value_at`'s first-present-page semantics. The paged branch of
  `SegmentStore::read_point` (which powers `GET …/storage/{aspect}/at` for paged frames) now takes
  it. Refactored the single-block reader onto the same section helper so both share one code path;
  runtime-verified against the live endpoint on a 5-page FOR frame.
- [ ] **Next slice — block-random-access timestamp search for the sorted point lookup (Phase 4/6):**
  the streaming point read no longer decodes the value column, but it still decodes the *whole*
  timestamp column to binary-search for the row — the remaining `O(n)` cost. Make the timestamp
  lookup block-random-access too (a binary search that reconstructs only the delta-of-delta blocks
  it probes) so a sorted point lookup is fully sublinear, and benchmark it vs the current
  whole-timestamp decode.
- [x] Add p50/p95/p99 + confidence-interval reporting
- [x] Add physical value types (`F64`, `ScaledI64`, `BigDecimalText` + three more)
- [x] Prototype columnar segment reads for one aspect type (`database::SegmentStore`)
- [ ] Publish a methodology document **before** any performance claim

> The key commercial move is not adding features — it is making DSP's performance
> claims measurable, reproducible, and valuable to a specific buyer. If DSP can
> credibly show it is faster, cheaper, or more accurate for large-scale interpolation
> and compression-aware irregular time-series analysis than general-purpose TSDBs, it
> has a clear path to a commercial product.
