# DSP

**A high-performance, GPU-accelerated, interpolation-native time-series database and analytics platform written in Rust.**

![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)
![Rust](https://img.shields.io/badge/Rust-nightly-orange.svg)
![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-blue.svg)
![Status](https://img.shields.io/badge/status-active%20development-green.svg)

DSP ingests raw, irregularly-sampled time-series measurements (sensor readings,
financial prices, telemetry, …), stores them with arbitrary-precision values in
its own typed columnar segment format, and turns them into insight: it
**interpolates** data to any resolution using GPU/CPU/SIMD spline engines,
**downsamples** into grid-aligned aggregates, **extracts recurring patterns**,
**detects events**, **correlates** the two, and **generates prediction signals** —
served over a benchmark-grade **HTTP API** with JSON / CSV / Arrow / Parquet /
InfluxDB-Line-Protocol interchange, driven from an interactive terminal UI, or
used directly as a set of Rust libraries. Every performance claim is backed by
the reproducible, correctness-gated **DSP-Bench** harness.

This document describes what DSP **does today**. The work queue — everything
planned, in progress, or discovered — lives in [`ROADMAP.md`](ROADMAP.md), the
single source of truth for status and priorities.

---

## Table of Contents

- [Overview](#overview)
- [Key Features](#key-features)
- [Architecture](#architecture)
- [Data Model](#data-model)
- [The Analytics Pipeline](#the-analytics-pipeline)
- [Interpolation Engine](#interpolation-engine)
- [Physical Types & Columnar Storage](#physical-types--columnar-storage)
- [HTTP Server](#http-server)
- [Benchmark Harness](#benchmark-harness)
- [Dataset Compression](#dataset-compression)
- [Getting Started](#getting-started)
- [Configuration](#configuration)
- [Library Usage](#library-usage)
- [Terminal UI](#terminal-ui)
- [Testing & Benchmarks](#testing--benchmarks)
- [Project Layout](#project-layout)
- [Performance Notes](#performance-notes)
- [Roadmap](#roadmap)
- [License](#license)

---

## Overview

DSP is organized as a Rust **Cargo workspace**, from a low-level numerical engine
up to an HTTP server and an interactive application:

| Crate | Role |
|-------|------|
| [`splimes`](splimes) | Spline interpolation engine — Linear / Quadratic / Cubic / Polynomial methods with automatic GPU, parallel, SIMD, and CPU strategy selection. |
| [`database`](database) | Time-series database: [Turso](https://turso.tech/) (libSQL) **control plane** (catalog, metadata, segment index; MVCC concurrent writes) + DSP's own typed columnar **`.dspseg` segment store** on the measurement hot path, plus the pattern-recognition types and tiered dataset compression. |
| [`database_orchestration`](database_orchestration) | High-level pipeline that chains batching → pattern extraction → event detection → correlation → signal generation, with built-in detectors and parallel execution. |
| [`dsp-physical-type`](dsp-physical-type) | Vendor-neutral physical type system — schema-declared numeric encodings with explicit exactness, timestamp codecs, and the `.dspseg` columnar segment format (single-block and paged). |
| [`dsp-arrow`](dsp-arrow) / [`dsp-arrow-store`](dsp-arrow-store) | Apache Arrow / Parquet interchange for sealed segments and stored reads, kept in leaf crates so the `arrow-*` dependency tree never reaches the hot-path core. |
| [`dsp-line-protocol`](dsp-line-protocol) | Dependency-free InfluxDB Line Protocol parser shared by the server and the benchmark harness. |
| [`dsp-reduce`](dsp-reduce) | Vendor-neutral downsampling reductions, computable over parts and merged (`reduce_partial`/`PartialReduction`, exact for every reduction) — `min`/`max`/`avg`/`sum`/`first`/`last` + nearest-rank `p50`/`p90`/`p95`/`p99` + three time-weighted averages (LOCF, linear/trapezoidal, LOCF-to-bucket-end) + mergeable bounded-error `sketch_p*` percentiles, over an epoch-aligned bucket grid, computed in `BigDecimal`. A `PartialReduction` is **serde-serializable** (so a segment's partial can be persisted and merged later, in place of re-reading it) and **re-bucketable** to any coarser nesting resolution (`rebucket`/`grids_nest`). Shared by the HTTP `downsample` endpoint and the benchmark harness. |
| [`dsp-server`](dsp-server) | Benchmark-grade `axum` HTTP API — interpolation, downsampling, storage ingest/query, catalog management, Prometheus metrics, live latency profiles. |
| [`dsp-bench`](dsp-bench) | Reproducible, correctness-gated benchmark harness — the roadmap's spine; both an internal suite and a customer-runnable diagnostic. |
| [`dsp-tui`](dsp-tui) | Terminal user interface (Ratatui + Crossterm) for creating databases, importing CSV data, browsing and plotting aspects, and running compression. |

The platform is designed for **real-time and large-scale** workloads: measurements
are stored with [`BigDecimal`](https://docs.rs/bigdecimal) logical precision,
interpolation transparently scales from a handful of points to millions across the
GPU, and the storage layer uses bulk transactions, cached connections, and typed
columnar segments throughout.

`legacy/` holds the 15 archived predecessor repositories (retained for reference
and full history, excluded from the workspace).

---

## Key Features

- **Arbitrary-precision time-series storage** — values are logically `BigDecimal`,
  timestamps are UTC, and resolution can be anything from **nanoseconds to years**.
- **Schema-declared physical encodings, never a silent downcast** — each aspect
  declares its physical encoding (`F64`, `F32`, `ScaledI64`, `ScaledI128`,
  `Decimal128`, `BigDecimalText`) and a permitted per-value error bound; a value
  the encoding cannot represent within tolerance is **rejected, not downcast**.
- **Typed columnar segment store (`.dspseg`)** — append-friendly,
  immutable-after-seal, CRC-checksummed, page-indexed segments with per-segment and
  per-page min/max statistics for data skipping, a null/quality column, and
  measured **bytes/point** rollups.
- **Hierarchical data model** — `Database → Subject → Aspect → Measurement`, with
  each aspect getting its own isolated set of storage files on disk.
- **Adaptive interpolation** — the engine automatically picks GPU, parallel (Rayon),
  SIMD (`wide`), or single-threaded CPU execution based on dataset size, with
  graceful method fallback (Cubic → Quadratic → Linear) when data is sparse.
- **GPU acceleration** — compute shaders (WGSL) run over [`wgpu`](https://wgpu.rs/)
  on Vulkan, Metal, or DX12, with buffer pooling, persistent staging buffers, and
  f64/f32 precision paths.
- **Benchmark-grade HTTP API** — interpolate, downsample, ingest, and query over
  REST with **JSON, CSV, Arrow IPC, Parquet, and InfluxDB Line Protocol** in and
  out; Prometheus metrics with latency histograms; a live p50/p95/p99 profile
  endpoint.
- **Provenance-labelled output** — every reconstructed point is marked `raw`,
  `interpolated`, or `extrapolated`, so a consumer never silently treats a
  synthetic value as an observed one.
- **Reproducible benchmarking** — DSP-Bench emits seeded, correctness-gated,
  accuracy-scored comparisons (DSP vs portable baselines) with bootstrap
  confidence intervals, JSON + HTML reports, and honest negative results.
- **Pattern recognition & prediction** — extract recurring shapes into dictionaries,
  detect events (peaks, valleys, threshold crossings, monthly increases, drawdowns),
  correlate patterns with events, and emit probability-weighted prediction signals.
- **Tiered dataset compression** — shrink historical data by time (older = more
  compressed) and/or to a target size, with progress reporting and dirty-region
  re-compression.
- **MVCC concurrency with cached connections** — concurrent writes via
  `BEGIN CONCURRENT` transactions over a TTL/LRU-cached connection pool.
- **Interactive TUI** — drive the platform from the terminal, including live
  CSV import progress, plotting, and compression dashboards.

---

## Architecture

```text
┌──────────────────────────────┐  ┌──────────────────────────────────────┐
│           dsp-tui            │  │              dsp-server               │
│  Ratatui terminal UI (app)   │  │  axum HTTP API: interpolate ·         │
│  create DB · import · plot   │  │  downsample · storage ingest/query ·  │
│  · compress                  │  │  catalog · /metrics · /debug/profile  │
└──────────────┬───────────────┘  └──────┬───────────────────┬───────────┘
               │ uses                     │ uses              │ columnar I/O
               ▼                          ▼                   ▼
┌──────────────────────────────┐   ┌────────────────────────────────────┐
│    database_orchestration    │   │   dsp-arrow / dsp-arrow-store      │
│  batch → patterns → events   │   │   Arrow IPC + Parquet interchange  │
│  → correlations → signals    │   └──────────────────┬─────────────────┘
└───────────────┬──────────────┘                      │
                ▼                                     ▼
┌─────────────────────────────────────────────────────────────────────┐
│                              database                                │
│   Turso/libSQL control plane (catalog · metadata · segment index)   │
│   + typed columnar `.dspseg` segment store (measurement hot path)   │
│   Subject/Aspect/Measurement · Patterns · Events · Compression      │
└───────────────┬─────────────────────────────────────┬───────────────┘
                │ physical types + segments           │ numerics
                ▼                                     ▼
┌──────────────────────────────┐   ┌──────────────────────────────────┐
│      dsp-physical-type       │   │             splimes              │
│  encodings · schemas ·       │   │   Spline interpolation engine    │
│  timestamp codecs · .dspseg  │   │  GPU (wgpu/WGSL) · Rayon · SIMD  │
└──────────────────────────────┘   └──────────────────────────────────┘
```

`dsp-bench` drives the whole stack from outside through the same public APIs a
customer would use; `dsp-line-protocol` is the shared ILP dialect between the
harness and the server. Dependencies flow downward; the `arrow-*` tree lives only
in the `dsp-arrow*` leaf crates.

---

## Data Model

DSP organizes data hierarchically. Each level maps to a directory or database file
on disk, so an aspect's storage is fully self-contained.

| Concept | Description | Example |
|---------|-------------|---------|
| **Database** | Top-level container for all data. | `Crypto`, `my_sensors` |
| **Subject** | A logical entity being observed. | `BTCUSD`, `temperature_sensor_001` |
| **Aspect** | A specific measurement type of a subject, with its own resolution, declared physical encoding, and (optional) compression config. | `open`, `ambient_temp` |
| **Measurement** | A timestamped, arbitrary-precision value. | `(2024-01-01T12:00Z, 22.5)` |

### On-Disk Layout

```text
{data_dir}/
└── {database_name}/
    ├── metadata.db                  # database metadata
    └── {subject_name}/
        └── {aspect_name}/
            ├── measurements.db        # raw measurements
            ├── unprocessed_batches.db # batching queue
            ├── processed_batches.db
            ├── patterns.db            # extracted patterns
            ├── events.db              # detected events
            ├── correlations.db        # pattern↔event links
            ├── pipeline.db            # persisted pipeline config & state
            └── dictionaries/
                └── {dictionary_name}.db
```

The **Storage v2 segment store** (see
[Physical Types & Columnar Storage](#physical-types--columnar-storage)) adds, per
store root: `catalog.db` (database/subject/aspect hierarchy + declared schemas),
per-aspect `metadata.db` rollups, `segment_index.db` (the prune-before-open
segment index), and `segments/*.dspseg` (the typed columnar measurement files).

Higher-level analytics types build on this foundation:

| Type | Purpose |
|------|---------|
| **Batch** | A group of measurements processed together. |
| **Pattern** | An extracted recurring shape, with the occurrences where it appears. |
| **Dictionary** | A collection of patterns sharing similarity constraints. |
| **Event** | A detected occurrence with one or more *manifestations* (time spans). |
| **Correlation** | A link between a pattern and an event. |
| **Signal** | A probability-weighted prediction derived from a correlation. |

---

## The Analytics Pipeline

The `database_orchestration` crate provides a fluent **`Pipeline`** API. Each aspect
has exactly one pipeline, persisted to its `pipeline.db`, so runs are incremental
and conflict-free. A pipeline can host multiple dictionaries and multiple detectors.

```text
 Measurements ─▶ Batches ─▶ Processed Batches
                                   │
                                   ▼
                          Dictionaries (1..N)        Event Detectors (1..N)
                          extract patterns           peaks · valleys ·
                                   │                  thresholds · monthly · …
                                   ▼                          │
                               Patterns ───── correlate ──────┤
                                                              ▼
                                  Events ─▶ Correlations ─▶ Signals
                                                              │
                                                              ▼
                                          query_probability(event, time)
```

**Built-in event detectors** (in `database_orchestration::detectors`):

| Detector | Description |
|----------|-------------|
| `detect_monthly_increase` | Months where the value rises ≥ a threshold start-to-end. |
| `detect_peaks` / `detect_all_peaks` | Global / all local maxima. |
| `detect_valleys` / `detect_all_valleys` | Global / all local minima. |
| `detect_threshold_crossing_up` / `_down` | Upward / downward threshold crossings. |
| `detect_drawdown` | Significant value drops. |

You can also register **custom detectors** with the `event_detector_fn!` macro, and
run pipelines across many aspects in parallel with `run_all_pipelines` /
`run_subject_pipelines`.

---

## Interpolation Engine

`splimes` is the numerical heart of DSP. Given a set of `Point`s and a target time
range + resolution, it produces an interpolated series using one of four spline
methods, choosing the fastest execution strategy automatically. It interpolates
**and extrapolates** — single-instant lookups and full ranges — on CPU, SIMD, and
GPU.

### Spline Methods

| Method | Min. points | Notes |
|--------|-------------|-------|
| `Linear` | 2 | Straight-line interpolation. |
| `Quadratic` | 3 | Second-degree. |
| `Cubic` | 4 | Smooth third-degree splines. |
| `Polynomial(degree, bounds_factor)` | degree + 1 | Arbitrary degree with optional bounds damping. |

If a method needs more points than are available, DSP **falls back** to the next
simpler method automatically (`Cubic → Quadratic → Linear`), never upgrading beyond
what you requested.

### Strategy Selection

The `should_use_gpu()` heuristic routes each request to the best backend based on
input and estimated output size (thresholds derived from internal benchmarks):

| Dataset size | Strategy |
|--------------|----------|
| ≥ 5M points (or ≥ 2.5M outputs) | **GPU primary** (memory efficiency dominates) |
| ≥ 50K points (or ≥ 50K outputs) | **GPU, then parallel fallback** |
| 1K – 50K points | **CPU** (avoids synchronization overhead) |
| 100 – 1K points | **GPU streaming** (pipelining overlaps well) |
| < 100 points | **CPU** (avoid all setup overhead) |

The GPU path uses [`wgpu`](https://wgpu.rs/) compute shaders with:

- **Buffer pooling** — size-tiered (4 KB–128 MB), LRU-evicted pool used throughout
  the interpolation paths (including the static f64/f32 entry points); cut
  per-run allocations from 7,000+ to under 50 on a 1M-point run and reuses
  buffers across batches.
- **Persistent staging buffers** — 3-buffer round-robin with persistent mapping
  that eliminates unmap/remap overhead.
- **f64 / f32 precision paths** — automatically chosen by GPU capability.
- **Pre-warming** — `splimes::prewarm_gpu()` (or the `gpu-eager-init` feature)
  removes first-call initialization latency; `GpuConfig` presets (`minimal()`,
  `low_memory()`, `default()`, `high_performance()`) size the pool, staging
  buffers, and command batch.

```rust,ignore
use splimes::{auto_interpolate, Resolution, Spline};

// Automatically selects GPU/CPU/parallel based on size:
let series = auto_interpolate(&mut points, start, end, Resolution::Seconds, Spline::Cubic).await?;
```

---

## Physical Types & Columnar Storage

DSP's storage hot path is its own typed columnar format; Turso/libSQL serves as
the **control plane** (catalog, metadata, segment index, pipeline state) and never
holds bulk measurements. `BigDecimal` remains the logical/API type everywhere.

### Physical type system (`dsp-physical-type`)

- **Six numeric encodings** — `F64`, `F32`, `ScaledI64`, `ScaledI128`,
  `Decimal128`, `BigDecimalText` — each declaring storage width, hot-path/GPU
  eligibility, and conversion behavior. Every `encode` reports an explicit
  **`Exactness`** (exact / lossy-with-measured-residual); `to_logical` is always
  available.
- **No silent downcast, enforced** — `AspectSchema` declares, per aspect, the
  physical encoding, a permitted per-value error bound, and the timestamp unit.
  Sealing a segment under a declared schema **errors** (`SealError::Encode` /
  `SealError::ToleranceExceeded`) rather than losing precision beyond the
  declared bound. Over HTTP this surfaces as a `400` — never a downcast.
- **Advisory encoding selection** — `recommend_encoding` picks the narrowest safe
  encoding within a tolerance, and columnar estimators report expected
  **bytes/point** (the north-star cost term) before anything is written.
- **Timestamp codecs** — integer-epoch timestamps (`TimeUnit` =
  seconds/millis/micros/nanos) with lossless **delta** and **delta-of-delta**
  transforms and **five** interchangeable second-difference codecs — per-value
  zig-zag + LEB128 **varint**, **RLE**, **fixed-width bit-packing**, a
  **Gorilla-style variable-length** codec, and **per-block adaptive (dynamic)
  bit-packing** — chosen per column by a smallest-wins selector. Each wins a different
  regime: bit-packing on regular/small-jitter series, RLE on long constant runs, Gorilla
  on **scattered single jitter** (isolated moderate spikes among regular intervals, where
  RLE cannot form runs and bit-packing must widen every value), and per-block adaptive
  bit-packing on a **mixed-magnitude** stream (a contiguous wide region among narrow runs,
  where a single global width overpays — 184 B vs 384–961 B for the other codecs on a
  256-value mixed corpus). A regular 1000-point column packs to ~12 bytes total.
- **Value-column codecs** — a value column is stored under its schema-declared physical
  encoding, and a **`ScaledI64`** column additionally chooses, via a self-describing
  selector byte, between a per-value zig-zag **varint**, **fixed-width bit-packing** of its
  mantissas, **per-block adaptive (blocked) bit-packing**, and **per-block
  Frame-of-Reference (FOR)** packing — bit-packing a regular scaled series (e.g. a
  2-decimal price ramp) to **37% below** the varint, the blocked codec confining a wide
  burst to the blocks it spans on a **mixed-magnitude** column, and FOR (subtract each
  block's minimum, pack the *unsigned* residual to the block's range — the standard
  FastLanes/ALP move) collapsing values **clustered at a high base** (a sensor reading
  near a fixed offset: mantissas near 1e9 store **88% below** the bit-packed
  alternative). Each codec is chosen only when strictly smallest, so a regular column is
  byte-for-byte unchanged; the realized figure is reported as
  `StorageEstimate.realized_value_bytes` / `value_codec`. The timestamp column keeps an
  *advisory* FOR estimate (`for_estimated_bytes`) — second differences are near-zero, so
  FOR rarely wins there.
- **Two-level delta cascade (opt-in)** — a **trending** `ScaledI64` column (a counter or
  monotone sensor whose magnitude every single-level codec pays for) is additionally
  compressible by a *cascade*: delta-transform the mantissas, then pack the differences with
  the smallest inner packer (varint / bit-pack / blocked / FOR / RLE). It is realized on disk
  as the `VAL_CODEC_DELTA_CASCADE` codec-chain descriptor (`write_value_column_cascading`,
  read back by the ordinary reader) and surfaced advisory-first in the bench
  (`StorageEstimate.advisory_delta_cascade_value_bytes`, schema v15). It is **opt-in** — the
  cascade beats even FOR broadly, so folding it into the default selector is a headline change
  held for owner sign-off; the default codec choice is unchanged.
- **Block-level random access** — the fixed-layout value codecs support decoding a single value
  (or a sub-range) without materializing the whole column: `dspseg::read_value_at(bytes, i)`
  reads only the block covering row `i` (skipping earlier blocks by their headers) for the
  blocked/FOR codecs, and reads bit `i * width` directly for the fixed-width bit-pack codec — the
  point-lookup / late-materialization lever. `dspseg::read_segment_point(bytes, t)`
  wires this up to the framed single-block segment — it skips the value block by its framing,
  decodes only the timestamps to find the row, and unpacks the one covering value block — so a
  point lookup **never materializes the value column** on a per-block codec (equal to a full
  decode + `value_at` for every frame; the non-block codecs fall back to that). `read_paged_segment_point`
  does the same for a paged frame, first pruning pages on their indexed min/max timestamp so only
  the surviving page is touched. A **regular (constant-stride) timestamp column** is resolved in
  closed form — `ts[i] = first + i*step`, so the row for an instant is `O(1)` with no timestamp
  materialization at all. Measured **~62× faster** point lookup on a 100k-row single-block FOR
  segment (192 µs vs 11.9 ms) and **~7.1× faster** on the paged frame (169 µs vs 1.20 ms), with the
  closed-form timestamp path a further **~7×** over an irregular column (192 µs vs 1.34 ms), identical
  bytes on disk ([`dsp-physical-type/benches/pointread.rs`](dsp-physical-type/benches/pointread.rs)).
- **Checkpointed frames — sublinear point lookup on an *irregular* sorted column** *(opt-in;
  `write_segment_checkpointed` / `write_paged_segment_checkpointed`)*. The closed form above only
  serves a *regular* column; an irregular one has no closed form, so a probe had to decode the whole
  delta-of-delta timestamp stream. A checkpointed frame persists a sparse `(row, timestamp, delta)`
  index every `stride` rows next to a range-decodable per-block dod stream, so a probe binary-searches
  the index and decodes **only the blocks it walks** — the timestamp column is never materialized.
  Measured **~3.45× faster** point reads on a 100k-row sorted-irregular frame (352.94 ms → 102.42 ms
  for 64 probes) for **+0.41% frame bytes** at stride 1024; a *paged* frame gains only **~1.14×**
  (68.70 → 60.41 ms), because page pruning already bounds its decode to `rows_per_page`
  ([`dsp-physical-type/benches/dodsearch.rs`](dsp-physical-type/benches/dodsearch.rs)). The codec tag
  is **additive** — every previously written frame still reads, with no format-version bump — and a
  checkpointed frame is byte-for-byte equivalent in behaviour to a plain one on every read.
  **Opt in** by setting `DSP_SEGMENT_CHECKPOINT_STRIDE` (see the configuration table). The seal path
  then writes the index only where it genuinely pays, on three counts: the column is **sorted and
  irregular** (a *regular* one already resolves `O(1)` closed-form, an out-of-order one cannot be
  binary-searched), it has at least `DSP_SEGMENT_CHECKPOINT_MIN_ROWS` rows, and the **codec override
  is affordable** — a checkpointed frame must use the range-decodable per-block codec, which costs
  ~3.5× on a Gorilla-shaped column and ~14× on an RLE-shaped one, so those are refused rather than
  bloated (`DSP_SEGMENT_CHECKPOINT_MAX_CODEC_OVERHEAD`). It is **off by default**, so an unconfigured
  store's bytes/point are unchanged; making it the default is an open roadmap decision.
- **Realized headline bytes/point** — every headline bytes/point figure
  (`Segment::bytes_per_point`, `StorageEstimate.bytes_per_point` /
  `total_bytes_per_point`, the bench HTML `val B/pt`) reports the codec **actually
  written to disk**, not a fixed-width estimate (bench schema v10). The naive
  `len * width` figure is retained beside it (`Segment::logical_value_bytes`,
  `StorageEstimate.estimated_value_bytes`) as the uncompressed baseline, so
  `realized / logical` reads directly as the storage compression ratio.

### Segment store (`.dspseg` + `database::SegmentStore`)

- **Sealed, checksummed segments** — a `Segment` binds a typed value column and a
  delta-of-delta timestamp column with min/max timestamp/value stats, row/null
  counts, and a format version. The on-disk frame is hand-rolled, versioned, and
  **CRC-32-verified before parse**, so a corrupt or truncated file fails fast.
- **Multi-codec timestamps** — the timestamp block writes its second-difference
  stream under whichever of **five** codecs is smallest — fixed-width **bit-packing**,
  per-value zig-zag **varint**, **RLE**, **Gorilla** variable-length, or **per-block
  adaptive (dynamic) bit-packing** — chosen by a self-describing selector byte the reader
  dispatches on, routed through the single source of truth so the reported codec name
  always matches the bytes on disk. A regular series packs to a handful of bytes on disk
  (a 1000-point regular block stores in <20 bytes), so the bytes/point saving is
  *realized*, not just estimated. The **value block** carries its own codec selector too,
  so a `ScaledI64` column stores its mantissas under whichever of **varint**, **fixed-width
  bit-packing**, **per-block adaptive (blocked) bit-packing**, or **per-block
  Frame-of-Reference (FOR)** packing is smallest on disk.
- **Order semantics** — every segment records whether its timestamps are monotonic
  (`time_sorted`). Ingest can *enforce* order (`require_sorted` rejects an
  out-of-order batch rather than sealing it), and the per-aspect/store rollups carry
  an `unsorted_segments` count so out-of-order data is visible before it costs a
  point-lookup scan.
- **Order-signal read planner** — a single-instant point lookup
  (`SegmentStore::read_point`) prunes the index to the segments spanning the instant
  and resolves each with its persisted `time_sorted` flag: a sorted segment is
  **binary-searched**, an out-of-order one linear-scanned (the only sound search on
  unsorted timestamps). Both frame kinds resolve through the **streaming point read**
  (`dspseg::read_segment_point` / `read_paged_segment_point`, below) — no value-column
  materialization on a per-block codec, and a paged frame **prunes pages on their indexed
  min/max timestamp without decoding a column byte** before touching the one surviving page.
  `SegmentStore::read_points` resolves a **batch** of instants in one pass — the index is pruned
  once and each segment's timestamp column decoded once for the whole batch, so `N` instants
  sharing a segment cost one decode, not `N` (**~27× faster** for 64 instants on a 100k-row FOR
  frame — 452 µs vs 12.2 ms,
  [`dsp-physical-type/benches/pointread.rs`](dsp-physical-type/benches/pointread.rs)).
- **Intra-segment reconciliation** — `SegmentStore::reconcile_segment`/`reconcile_aspect`
  rewrite an out-of-order segment into a sorted one in place (stable sort by
  timestamp, re-sealed at the same id, frame kind preserved), so it drops out of the
  `unsorted_segments` count and its point lookups binary-search.
- **Threshold-triggered reconciliation** — the `unsorted_segments` backlog drives a
  QuestDB-style trigger so the rewrite is paid only once out-of-order data is worth
  compacting, never on every late row: `reconcile_aspect_if_unsorted_exceeds` gates a
  single-aspect pass on the backlog, `reconcile_all_over_threshold` sweeps every
  declared aspect over the threshold, and a background timer daemon
  (`DSP_RECONCILE_INTERVAL_SECS` / `DSP_RECONCILE_THRESHOLD`) runs the sweep
  unattended. A threshold of 0 clamps to 1 (any out-of-order segment). Passes and the
  segments they rewrite are counted in `dsp_reconcile_*` metrics.
- **Hot/cold reconciliation** — `reconcile_aspect_hot_cold`/`reconcile_all_hot_cold`
  reconcile every *cold* (sealed) out-of-order segment on each pass but defer the
  *hot tail* (the most-recently-sealed segment) until the backlog reaches the
  threshold, so the actively-appended segment is not rewritten on every tick —
  QuestDB's "squash non-active partitions each commit, defer the active one" policy.
  Enabled on the daemon with `DSP_RECONCILE_HOT_COLD` and on the reconcile endpoints
  with `?hot_cold=true`.
- **Cross-segment overlap signal** — a *sorted* segment whose time window a
  later-sealed segment re-enters (late data landing in an already-covered window) is
  not counted by `unsorted_segments`; `SegmentIndex::overlapping_count` reports these
  time-**overlapping** segments as a distinct `overlapping_segments` order-health
  count in the per-aspect and store-wide rollups.
- **Cross-segment overlap merge** — `SegmentStore::reconcile_overlaps`/`reconcile_all_overlaps`
  collapse each connected group of time-overlapping segments into one time-sorted
  segment, folding members oldest→newest with **newer-wins** (last-writer-wins /
  upsert) dedup on shared timestamps — the same answer a point lookup already gives
  across overlaps, and QuestDB's `DEDUP UPSERT` "last write wins on the designated
  timestamp" semantics. Drives `overlapping_segments` to zero; exposed on the reconcile
  endpoints with `?overlaps=true` and as a background daemon sweep
  (`DSP_RECONCILE_OVERLAPS`).
- **Split-not-rewrite merge** — `SegmentStore::split_segment` carves a sorted segment
  at a boundary into a cold prefix (kept id) + hot suffix (new id), and
  `reconcile_overlaps_with_policy` consults a size-based `SplitPolicy`: when an overlap
  component's cold prefix clears a byte floor **and** outweighs its hot suffix, only the
  suffix is merged and the large cold prefix is left untouched, so later late arrivals
  re-entering the hot window never rewrite it — QuestDB's partition-split write-amp
  bound. The default `reconcile_overlaps` keeps QuestDB's 50 MiB floor (small components
  full-rewrite); a floor is selectable with `?split_min_bytes=` on the per-aspect and
  store-wide reconcile endpoints and `DSP_RECONCILE_SPLIT_MIN_BYTES` on the daemon.
- **Squash** — `SegmentStore::squash_aspect`/`squash_aspect_if_exceeds`/
  `squash_all_over_threshold` fold an aspect's segments back into one (newer-wins),
  bounding the fragmentation repeated split carve-offs create — QuestDB's
  `max.splits` squash trigger. Exposed as `POST …/{aspect}/squash?max_segments=` and on
  the daemon with `DSP_RECONCILE_MAX_SPLITS` (squash aspects over the cap each tick,
  after the overlap merge that accumulates the splits).
- **Null/quality column** — a per-row presence bitmap (zero bytes for fully dense
  columns) makes gaps real: present values are stored densely and reconstructed
  as `None` on read.
- **Paged segments** — rows partition into fixed-height pages, each independently
  encoded with its **own stats**, behind a per-page index table: a reader prunes
  pages on min/max time/value **without touching a column byte** and seeks
  straight to the pages it needs.
- **Data skipping** — segment-level and page-level pruning by time, by value
  band, and by **quality** (segments/pages that overlap a window but hold only
  nulls there are skipped). A **range read over a regular (constant-stride)
  block-coded segment** goes further — `read_segment_range` computes the row
  window in closed form and unpacks only the present values inside it, so a
  selective range decodes ~`window` values, not the whole segment (**~20× faster**
  for a 100-row window over a 100k-row FOR frame — 629 µs vs 12.7 ms,
  [`dsp-physical-type/benches/pointread.rs`](dsp-physical-type/benches/pointread.rs)).
- **Control plane** — `SegmentIndexStore` persists one descriptor per sealed
  segment in libSQL and answers range queries with SQL pruning;
  `CatalogStore`/`AspectCatalog` register the database → subject → aspect
  hierarchy with declared schemas (declare once, seal by lookup — an undeclared
  aspect is refused, not guessed); per-aspect `metadata.db` rollups make
  aspect-wide stats (including realized **bytes/point**) an O(1) read, always
  re-derivable from the segment index.

### Arrow & Parquet interchange (`dsp-arrow`, `dsp-arrow-store`)

- **Lossless Arrow batches** — sealed segments and stored reads convert to/from
  Apache Arrow `RecordBatch`es: a lossless text form, plus a **typed fast path**
  (`F64 → Float64`, `F32 → Float32`, scaled integers → **exact `Decimal128`**).
  Paged segments stream one batch per page.
- **Wire formats** — Arrow **IPC stream** bytes and **Parquet** files (embedding
  the Arrow schema so DSP's time-unit/encoding metadata survives), both
  directions: export a stored range, or ingest a Parquet file into a declared
  aspect under the full no-silent-downcast guarantee.
- **Lean core preserved** — the `arrow-*`/`parquet` dependency tree lives only in
  these leaf crates; `splimes`/`database`/`dsp-physical-type` stay arrow-free.

---

## HTTP Server

`dsp-server` is the benchmark-grade `axum` HTTP surface — DSP-Bench, TSBS-style
harnesses, Grafana, and SDKs drive the engine through it. It carries no
vendor-specific dependencies.

```sh
cargo run -p dsp-server
# dsp-server v0.1.0 listening on http://127.0.0.1:8080
```

Bind address defaults to `127.0.0.1:8080` (`DSP_SERVER_ADDR` overrides). Setting
`DSP_SEGMENT_STORE_ROOT` opens a Storage v2 segment store and enables the
`/storage` endpoints (without it they answer `503`, and `GET /ready` reports the
`segment_store` dependency).

### Service endpoints

| Method & path | Purpose |
|---------------|---------|
| `GET /health` | Liveness. |
| `GET /ready` | Readiness, including the segment-store dependency check. |
| `GET /metrics` | Prometheus text exposition — request/error/output counters, ingest counters (`dsp_ingest_*`), and **latency histograms** for the compute and storage-ingest paths. |
| `GET /debug/profile/current` | Live p50/p95/p99 latency snapshot per instrumented path. |

### Compute endpoints

The flagship interpolation-on-read path and its reduction counterpart. Each
accepts JSON points or an InfluxDB-Line-Protocol body, and each returns **JSON,
CSV, Arrow IPC, or Parquet**:

| Endpoint | Purpose |
|----------|---------|
| `POST /api/v1/interpolate` | Reconstruct an irregular series onto a regular grid (`spline` = `linear` \| `quadratic` \| `cubic` \| polynomial; `resolution` = `nanoseconds`..`years`). Every output point carries a `kind` — `raw` / `interpolated` / `extrapolated`. |
| `POST /api/v1/interpolate/point` | Evaluate the reconstructed signal at a single instant, labelled raw/interpolated/extrapolated. |
| `POST /api/v1/downsample` | Reduce samples into epoch-grid-aligned buckets — `min`/`max`/`avg`/`sum`/`first`/`last`, nearest-rank percentiles `p50`/`p90`/`p95`/`p99`, the three **time-weighted averages** (`twa`, LOCF dwell-weighting, for change-only sensors; `twa_linear`, trapezoidal `Σ½(vᵢ+vᵢ₊₁)Δtᵢ/ΣΔtᵢ`, for irregularly-sampled continuous signals; `twa_bucket_end`, LOCF that additionally carries the bucket's last sample to its grid end — `twa` gives that sample no weight, so a sensor reporting `0` then `100` a minute into an hour bucket reads `twa`=0 but `twa_bucket_end`=98.33), and the **approximate `sketch_p50`/`p90`/`p95`/`p99`** (see below); all reductions computed in `BigDecimal` by the shared [`dsp-reduce`](dsp-reduce) crate. Aliases: `median`→`p50`, `time_weighted_avg`/`twa_locf`→`twa`, `time_weighted_avg_linear`→`twa_linear`, `twa_locf_end`→`twa_bucket_end`, `sketch_median`→`sketch_p50`. Only non-empty buckets are emitted. |
| `POST /api/v1/{interpolate,downsample}/ilp` | The same, fed an ILP `text/plain` body (the TSBS/InfluxDB/QuestDB wire format); `field`, `precision` (`ns`/`us`/`ms`/`s`), and the compute knobs are query parameters. `interpolation=` is accepted as an alias for `spline=` (the canonical `spline` wins if both are given). |
| `POST /api/v1/{interpolate,downsample}/{csv,arrow,parquet}` and `…/ilp/{csv,arrow,parquet}` | The same computations with CSV (`text/csv`), Arrow IPC stream, or Parquet output — so a harness feeding line protocol pulls results in any of the four formats. |

**What the reductions cost.** Measured on the shipped harness (500k points, 5 reps, hour buckets,
correctness PASS throughout — `dsp-bench --downsample --ds-points 500000 --ds-aggs <agg>`):

| reduction | p50 | throughput | note |
|---|---|---|---|
| `avg` | 293.1 ms | 1,711,287 points/sec | the streaming baseline — no bucket materialized |
| `twa` | 426.0 ms | 1,169,431 points/sec | 1.45× the streaming cost: dwell-weighting needs the time-ordered samples |
| `twa_bucket_end` | 430.7 ms | 1,167,829 points/sec | +1.1% over `twa` — one extra weight, effectively free |
| `twa_linear` | 538.6 ms | 928,112 points/sec | 1.26× `twa`: an extra `BigDecimal` add + divide per interval for the trapezoidal mean |

**Exact vs sketch percentiles — which to ask for.** The `p50`/`p90`/`p95`/`p99` reductions are
*exact* nearest-rank: they return an actual observed `BigDecimal` from the bucket, but they
materialize and sort the whole bucket, and two buckets' results cannot be combined. The
`sketch_p50`/`p90`/`p95`/`p99` reductions instead stream every value into a
[DDSketch](https://dl.acm.org/doi/10.14778/3352063.3352135) — a **1% relative-error** bound
(`dsp_reduce::SKETCH_ALPHA`), **bounded memory** regardless of bucket size (values fold in as they
arrive; `SKETCH_MAX_BINS` caps the store absolutely), and an **exactly mergeable** structure, so a
p99 can be computed over a large or streaming bucket. Measured on the shipped harness (500k points,
5 reps, correctness PASS): `sketch_p99` runs at **1,075,976 points/sec (p50 = 468.90 ms)** versus
exact `p99` at **249,169 points/sec (p50 = 2018.53 ms)** — **~4.3× faster**
(`dsp-bench --downsample --ds-points 500000 --ds-aggs sketch_p99` vs `--ds-aggs p99`).

The approximation is **declared, never silent** — DSP's precision principle. The sketch shares the
exact percentiles' **nearest-rank convention** (`⌈q·n⌉`), so `sketch_p*` and `p*` name the same
sample at *every* bucket size and the sketch is always within the 1% bound of the exact answer —
the two are substitutable. (DDSketch's reference rank is `⌊q·(n-1)⌋`, which on a small bucket picks
a different sample; DSP deliberately does not inherit that.) The exact percentiles remain the
default and cost nothing on a small bucket, so prefer them there; reach for a sketch when a bucket
is too large to materialize or the result must merge.

```sh
curl -s -X POST http://127.0.0.1:8080/api/v1/interpolate \
  -H 'content-type: application/json' \
  -d '{
        "spline": "linear",
        "resolution": "seconds",
        "points": [
          { "timestamp": "1970-01-01T00:00:00Z", "value": 0.0 },
          { "timestamp": "1970-01-01T00:01:00Z", "value": 60.0 }
        ]
      }'
```

```sh
printf 'cpu,host=a load=0 1000000000\ncpu,host=a load=60 1000000060\n' | \
  curl -s -X POST \
    'http://127.0.0.1:8080/api/v1/interpolate/ilp?field=load&precision=s&spline=linear&resolution=seconds' \
    -H 'content-type: text/plain' --data-binary @-
```

Malformed payloads, unknown tokens, fewer than two usable points, or a zero-span
series return `400` with an `{"error": "..."}` body.

**Numeric boundary:** the compute endpoints' wire values are `f64` — the
documented transport boundary (the logical `BigDecimal` is narrowed only at the
HTTP edge, and the columnar value columns are honestly typed `Float64` — no false
precision). The **storage** endpoints below are lossless.

### Storage endpoints (Storage v2 over HTTP)

Catalog management, ingest, and stored-range reads against the `.dspseg` segment
store. The no-silent-downcast guarantee holds end to end: a value unrepresentable
under the declared encoding/tolerance is rejected `400`.

| Endpoint | Purpose |
|----------|---------|
| `POST /api/v1/storage/aspects` | **Declare** an aspect's schema — physical encoding, value tolerance, timestamp unit. |
| `GET /api/v1/storage/aspects` · `…/{aspect}/schema` · `…/catalog` | List declared schemas; read one aspect's schema; report the store's `(database, subject)` scope + registered hierarchy. |
| `POST /api/v1/storage/{aspect}/points` | Ingest a JSON batch (dense or nullable; single-block or paged via `rows_per_page`). Set `require_sorted` to reject an out-of-order batch (`400`, naming the first backwards row) instead of sealing it. Every ingest response reports `time_sorted` (whether the sealed segment stored in monotonic order). |
| `POST /api/v1/storage/{aspect}/{ilp,parquet,csv}` | Ingest an ILP payload, a Parquet file, or CSV rows into a declared aspect — all sealing through the same schema-enforced path, and all honouring `require_sorted` (query param) + reporting `time_sorted`. |
| `GET /api/v1/storage/{aspect}/range` · `…/value-range` | Stream a stored time window / value band as **Arrow IPC** (`application/vnd.apache.arrow.stream`). |
| `GET …/range.parquet` · `…/value-range.parquet` | The same windows as **Parquet** files. |
| `GET …/range.csv` · `…/value-range.csv` | The same windows as **CSV** (lossless decimal-text values; empty field = null). |
| `GET /api/v1/storage/{aspect}/points` · `…/value-points` | Lossless JSON reads with declarative pagination — `offset`/`limit` (+ `take` and 1-based `page` aliases), with `total`/`count`/`offset` in the body. For stable forward iteration the body also carries an opaque **`next_cursor`** while rows remain; a client re-issues with `?cursor=<token>` (supersedes `offset`/`page`; malformed → `400`) until it is absent. |
| `GET /api/v1/storage/{aspect}/at?t=` | **Single-instant point lookup**: the present value at exactly `t` (lossless decimal text) or a `found:false` miss. An **order-signal-driven read planner** resolves each candidate segment with its persisted `time_sorted` flag — binary search on a sorted segment, linear scan only on an out-of-order one — after pruning the index to the files spanning `t`. |
| `GET /api/v1/storage/{aspect}/at-multi?t=,,` | **Batch point lookup**: a comma-separated list of instants resolved in one pass (`points:[{timestamp,value,found}]` in query order). The index is pruned once by the batch's whole span and each segment's timestamp column decoded once for the whole batch, so `N` instants sharing a segment cost one decode, not `N`. |
| `GET`·`POST /api/v1/storage/{aspect}/downsample?start=&end=&resolution=&agg=` | **Stored-range downsample** — the same reduction set as `POST /api/v1/downsample` (above), but over the aspect's **persisted segments** instead of a request-supplied series, so a client aggregates a large stored range without fetching it. **Bounded memory**: the index is pruned by time and each surviving segment folds into its own mergeable partial reduction, merged once — only one segment's rows are ever resident, so a range far larger than RAM reduces, and with a `sketch_p*` the per-bucket state is bounded too. Omitted bounds span all stored history. `…/downsample.csv` · `…/downsample.arrow` · `…/downsample.parquet` serve the identical reduction in the compute endpoints' columnar formats. Traced as a `downsample.range` stage span. The pruned segments are reduced **concurrently**: measured **4.69× faster** than the sequential fold (16 segments over 200k rows, 39.88 ms → 8.50 ms; `sketch_p99` 49.49 ms → 9.42 ms, **5.25×**) — see [`database/benches/downsample_range.rs`](database/benches/downsample_range.rs). A single pruned segment takes an inline fast path (concurrency there costs more than it buys). **Materialized partials (opt-in):** with `DSP_SEGMENT_PARTIAL_BASE` set, each segment carries a `.dspart` partial-reduction sidecar written at seal (a sealed segment is immutable, so its partial never goes stale), and a downsample of the bounded reductions (`min`/`max`/`avg`/`sum`/`first`/`last` + `sketch_p*`) **merges the stored partials instead of decoding the value column** — measured **3.2× faster** (16 segments / 200k rows, 8.39 ms → 2.60 ms, non-overlapping CIs) and re-keyed to any coarser nesting resolution (minutes→hours→days) with no decode. **Rollup tiers (opt-in):** `DSP_SEGMENT_PARTIAL_TIERS` materializes coarser tiers beside the base (e.g. `hours,days`), each re-keyed from the tier below, so a coarse query folds the coarsest matching tier instead of the whole fine base — the in-storage analogue of TimescaleDB's hierarchical continuous aggregates. Measured **~1.16×** on a DAY query over a MINUTES base (16 segments / 200k rows, 7.30 ms → 6.30 ms, non-overlapping CIs; the win grows with the base-bucket count per segment) — see [`database/benches/downsample_range.rs`](database/benches/downsample_range.rs). A reconcile/split/squash regenerates the sidecar (tiers and all) so the acceleration survives a rewrite. |
| `POST /api/v1/storage/{aspect}/reconcile` | **Reconciliation pass** (`mode` in the response). Default (intra-segment): rewrite every out-of-order segment of the aspect into a time-sorted one in place. `?threshold=N` gates it on `unsorted_segments >= N` (QuestDB-style split-count trigger). `?hot_cold=true` reconciles cold segments but defers the hot tail until the backlog reaches the threshold. `?overlaps=true` instead runs the **cross-segment overlap merge** (newer-wins) — `reconciled` is then the number of segments merged away, and `?split_min_bytes=N` makes that merge **split-not-rewrite** (carve off a dominant cold prefix instead of rewriting the whole component). Returns `triggered`/`reconciled`/`cold_reconciled`/`hot_reconciled` and the post-pass `unsorted_segments` + `overlapping_segments`. |
| `POST /api/v1/storage/{aspect}/squash` | **Squash** the aspect's segments into one (newer-wins), bounding split-path fragmentation. `?max_segments=N` gates it (squash only when the count exceeds `N`). Returns `triggered`/`removed`/`segment_count`. |
| `POST /api/v1/storage/{aspect}/compact?target_rows=N` | **Size-targeted compaction** — coalesce the aspect's segments toward ~`N` rows per segment (leaving already-large segments untouched), holding fragmentation near the read-optimal size rather than folding to one (which `squash` does). Motivated by the `downsample_range` knee (one giant segment reads slower than several mid-sized ones). `target_rows` is required (absent → `400`). Returns `removed`/`segment_count`. The manual counterpart of the `DSP_COMPACT_TARGET_ROWS` daemon pass. |
| `POST /api/v1/storage/reconcile` | **Store-wide reconciliation sweep** across every declared aspect: threshold (default), `?hot_cold=true`, or `?overlaps=true` (+ `?split_min_bytes=N` for split-not-rewrite). Returns `mode`, `aspects_scanned` / `aspects_reconciled` / `segments_reconciled` (+ cold/hot split) and the post-sweep store-wide `unsorted_segments` + `overlapping_segments`. The manual counterpart to the background reconcile daemon (`DSP_RECONCILE_INTERVAL_SECS` / `DSP_RECONCILE_THRESHOLD` / `DSP_RECONCILE_HOT_COLD` / `DSP_RECONCILE_OVERLAPS` / `DSP_RECONCILE_SPLIT_MIN_BYTES` / `DSP_RECONCILE_MAX_SPLITS`). |
| `GET /api/v1/storage/{aspect}/stats` · `…/storage/stats` | Materialized per-aspect and store-wide rollups, including the realized **bytes/point** (the north-star cost term), an `unsorted_segments` order-health count (segments that would force a linear scan on a point lookup), and an `overlapping_segments` count (time-overlapping segments — the cross-segment order-health signal, computed by an index scan). Rollup fields are served from the control plane without opening a segment. |

### Metrics

`GET /metrics` renders Prometheus text exposition (v0.0.4): shared
`dsp_interpolate_*` / `dsp_downsample_*` counters across each endpoint family,
`dsp_ingest_*` counters (requests/errors/rows/segments) on the ingest paths,
`dsp_reconcile_*` counters (`_passes_total` / `_segments_reconciled_total`) over the
out-of-order reconciliation passes (manual, store-wide, and the background daemon —
threshold-held calls excluded), and latency histograms over the compute and seal paths.
`GET /debug/profile/current` serves the same timing data as a live p50/p95/p99
snapshot.

---

## Benchmark Harness

DSP-Bench (`dsp-bench`) is DSP's reproducible, correctness-gated benchmark
harness — both an internal engineering suite and a public, customer-runnable
diagnostic. Every number it emits ships with the dataset seed, the workload
profile, and a correctness verdict.

> Policy: *DSP benchmarks guide real engineering decisions, not synthetic wins.*
> A latency number is only publishable when its correctness check passes.

What it does today:

- **Seeded workload profiles** — the flagship `interpolation-heavy-irregular`
  profile generates an irregularly-spaced, gap-containing series
  (`ChaCha8Rng`, published seed → byte-for-byte reproducible). The underlying
  analytic signal is shape-selectable (`MultiSine` | `Sawtooth` | `Step` |
  `DampedSine`) and doubles as the accuracy ground truth.
- **Storage, read & aggregation workloads** — beside the flagship interpolation
  run, four parallel workloads exercise DSP's own hot paths, each with its own
  correctness gate and p50/p95/p99 latency: **`point_lookup`** (`--point-lookup`)
  seals a `.dspseg` segment and times the streaming point read
  (`read_segment_point`/`read_segment_points`, single or batch, single-block or
  paged via `--pl-rows-per-page`, regular closed-form vs irregular), gated against
  the full-decode value; **`range_fetch`** (`--range-fetch`) times the windowed
  range read; **`compression`** (`--compression`) reports realized bytes/point, the
  value-column compression ratio, and decode throughput, gated on an exact
  round-trip; and **`downsample`** (`--downsample`) times DSP's canonical
  [`dsp-reduce`](dsp-reduce) reduction into grid-aligned buckets with a
  `--ds-aggs` selector over `min`/`max`/`avg`/`sum`/`first`/`last`/`p50`…`p99`/`twa`/`twa_linear`/`twa_bucket_end`/`sketch_p50`…`sketch_p99`.
  `--ds-parallel <N>` reduces in N chunks via mergeable partial reductions (identical
  buckets to serial, asserted by test) — measured **14.7× at 64 chunks** on a 16-core box
  (441.2 ms → 30.1 ms, 1,134,659 → 16,921,104 points/sec, `--ds-aggs sketch_p99`, 500k points,
  5 reps, correctness PASS).
  The underlying point/range read speedups are quantified at the codec layer in
  [`dsp-physical-type/benches/pointread.rs`](dsp-physical-type/benches/pointread.rs).
- **Vendor-neutral adapters** — every system is driven through the
  `SystemAdapter` trait: the DSP reference adapter (`splimes::auto_interpolate`),
  a precision-aware **portable linear baseline** (fair-protocol class C), and a
  **forward-fill/LOCF baseline** (class B — the in-process mirror of
  `FILL(previous)` / `locf()`).
- **Quality beside speed** — reconstructions are scored against the analytic
  ground truth: **RMSE / MAE / max-error / bias**. Honest finding, surfaced not
  hidden: on smooth `multisine` (and `step`) DSP's cubic leads on RMSE, but on
  `sawtooth` the portable linear baseline out-accuracies the cubic, which
  overshoots sharp discontinuities.
- **Fair-protocol statistics** — p50/p95/p99 + min/max/mean/stddev with
  **seeded bootstrap confidence intervals**, and an end-to-end timing breakdown
  proving the latency distribution times only the adapter call, never setup.
- **Storage estimate** — each result carries a `StorageEstimate` (recommended
  encoding + value/timestamp/total **bytes/point**) computed by
  `dsp-physical-type` over the actual columns. It also surfaces **advisory
  potential-saving** estimates for codecs not yet realized on disk: the best-of
  f64 value codec (Gorilla / Chimp / **Chimp128** XOR, `advisory_best_f64_bytes` /
  `_codec`) for a lossy `F64` column, and the **Sprintz FIRE** forecaster's
  footprint on the timestamp column (`advisory_fire_timestamp_bytes`) — each a
  *what-if* number the adopt-or-drop decision reads, never a realized headline
  claim.
- **Reports** — a `BenchReport` JSON artifact (run metadata + a best-effort
  hardware probe: CPU model, cores, RAM) under `reports/json/`, plus a
  self-contained **HTML** view (`--html`) with the most-accurate row highlighted.
- **ILP / TSBS input** — a `.lp` / TSBS file drives the same harness, correctness
  gate, and reporting via the shared `dsp-line-protocol` parser.

```sh
# Benchmark a line-protocol file, DSP vs the portable baselines, JSON + HTML report:
cargo run -p dsp-bench -- \
    --input data.lp --field usage --spline cubic --resolution minutes --compare --html

# Seeded synthetic mode — the only mode with a known ground truth, so it also scores accuracy:
cargo run -p dsp-bench -- \
    --synthetic --points 300 --noise 0 --shape sawtooth \
    --spline cubic --resolution minutes --reps 5 --compare

# Storage/read/aggregation workloads over DSP's own hot paths:
cargo run -p dsp-bench -- --point-lookup --pl-rows 100000 --pl-queries 128 --reps 50
cargo run -p dsp-bench -- --range-fetch --rf-window 100 --rf-windows 32 --reps 30
cargo run -p dsp-bench -- --compression --comp-shape clustered --reps 20
cargo run -p dsp-bench -- --downsample --ds-bucket m --ds-aggs min,max,p99,twa --reps 20
```

The process exits non-zero when any result's correctness gate fails, so scripted
callers can gate on it. External-engine competitor adapters (DuckDB, ClickHouse,
InfluxDB 3, QuestDB, TimescaleDB) are the next tracked step in
[`ROADMAP.md`](ROADMAP.md).

---

## Dataset Compression

Long-running deployments accumulate huge volumes of raw measurements. The
compression module (`database::compression`) reduces storage while preserving the
shape of the data, configured per-aspect via `CompressionConfig`:

- **Time-based** — recent data within a "pure" window (e.g. the last 7 years) stays
  uncompressed; older data is progressively compressed across configurable **tiers**
  with `Linear`, `Exponential`, or `Custom` aggressiveness scaling.
- **Size-based** — compress until total storage fits within a target (e.g. 10 GB),
  hitting the oldest data hardest. Takes precedence over time-based.

Compression works by interpolating to a coarser resolution and applying
slope-change simplification to drop redundant points. Newly inserted data inside a
compressed range marks a **dirty region** for targeted re-compression. Progress is
reported live (phase, tier, aggressiveness, time range) — surfaced in the TUI.

```rust,ignore
use database::{CompressionConfig, TimeBasedCompressionConfig};

let config = CompressionConfig::time_based(
    TimeBasedCompressionConfig::default_seven_years()
);
let aspect = db.track_aspect(&subject_id, "temperature", &Resolution::Hours, Some(config)).await?;
let summary = aspect.compress(&db).await?;
```

---

## Getting Started

### Prerequisites

- **Rust nightly toolchain.** The `database` crate uses
  `#![feature(stmt_expr_attributes)]` and `splimes` targets the **2024 edition**, so
  a recent nightly is required (developed against `1.96.0-nightly`).

  ```bash
  rustup toolchain install nightly
  rustup override set nightly   # run inside the repo
  ```

- **A GPU with a `wgpu` backend** (Vulkan / Metal / DX12) is recommended for
  acceleration but **optional** — the engine falls back to CPU/parallel automatically
  if no compatible GPU is present.

### Build

```bash
# Clone, then from the repo root:
cargo build --release            # build all workspace crates
cargo build --release -p splimes # build a single crate
```

### Run the HTTP server

```bash
cargo run --release -p dsp-server
# optionally: DSP_SEGMENT_STORE_ROOT=./store to enable the /storage endpoints
```

### Run the Terminal UI

```bash
cargo run --release -p dsp-tui
```

> **Tip:** by default, databases are created under a portable per-user directory
> (`~/.dsp/data`). Set `DSP_DATA_DIR` to relocate both the databases and the TUI log
> (see [Configuration](#configuration)).

---

## Configuration

DSP is configured primarily through environment variables:

| Variable | Used by | Purpose | Default |
|----------|---------|---------|---------|
| `DSP_DATA_DIR` | `database`, `dsp-tui` | Root directory for database files **and** the TUI log (`dsp-tui.log`). | portable per-user default (see below) |
| `TEST_DATA_DIR` | `database` | Highest-priority override for the database root (used by the test suite). | unset |
| `DSP_SERVER_ADDR` | `dsp-server` | HTTP bind address. | `127.0.0.1:8080` |
| `DSP_SEGMENT_STORE_ROOT` | `dsp-server` | Root of the Storage v2 segment store; enables the `/storage` endpoints. | unset (storage endpoints answer `503`) |
| `DSP_RECONCILE_INTERVAL_SECS` | `dsp-server` | Background reconcile daemon sweep interval in seconds; `0`/unset disables it. | unset (disabled) |
| `DSP_RECONCILE_THRESHOLD` | `dsp-server` | `unsorted_segments` backlog an aspect must reach before the daemon reconciles it. | `1` |
| `DSP_RECONCILE_HOT_COLD` | `dsp-server` | Truthy → the daemon reconciles cold segments each tick and defers the hot tail until the threshold. | unset (all-or-nothing) |
| `DSP_RECONCILE_OVERLAPS` | `dsp-server` | Truthy → each daemon tick also merges cross-segment time-overlap groups. | unset (disabled) |
| `DSP_RECONCILE_SPLIT_MIN_BYTES` | `dsp-server` | Split-not-rewrite floor (bytes) for the daemon's overlap merge; a dominant cold prefix clearing it is split off rather than rewritten. | unset (default 50 MiB floor → full-rewrite) |
| `DSP_RECONCILE_MAX_SPLITS` | `dsp-server` | Segment-count cap; each tick also squashes every aspect over it into one segment (bounds split-path fragmentation). | unset (no squash) |
| `DSP_COMPACT_TARGET_ROWS` | `dsp-server` | Target segment size in **rows** for the daemon's size-aware compaction pass; each tick coalesces every aspect's segments toward ~this many rows per segment (leaving already-large segments alone), holding fragmentation near the read-optimal size instead of folding to one (`DSP_RECONCILE_MAX_SPLITS`). Motivated by the `downsample_range` knee (one giant segment reads slower than several mid-sized ones). Needs the daemon interval set. | unset (no compaction) |
| `DSP_SEGMENT_CHECKPOINT_STRIDE` | `dsp-server` | Rows between entries of the sealed **timestamp checkpoint index** — trades a little size for much faster point lookups on **sorted, irregular** columns (~3.45× single-block; see the checkpointed-frames feature above). Applies only where it pays: sorted + irregular + at least `DSP_SEGMENT_CHECKPOINT_MIN_ROWS` rows. ~1024 is the sweet spot (stride barely moves speed but does move size). | unset (no index; frames byte-for-byte as before) |
| `DSP_SEGMENT_CHECKPOINT_MIN_ROWS` | `dsp-server` | Row floor below which a segment is never checkpointed (a small column decodes trivially, so an index would be pure cost). | `8192` |
| `DSP_SEGMENT_CHECKPOINT_MAX_CODEC_OVERHEAD` | `dsp-server` | Ceiling on the timestamp-codec override a checkpointed seal will accept (`blocked / best` bytes; `1.0` = only when free). A checkpointed frame must use the range-decodable per-block codec, which is ~free where that codec already wins but **~3.5× on a Gorilla-shaped and ~14× on an RLE-shaped column** — this refuses those seals rather than silently bloating them. | `1.25` |
| `DSP_SEGMENT_PARTIAL_BASE` | `dsp-server` | Resolution token (`seconds`/`minutes`/`hours`/…) at which each sealed segment materializes a **partial-reduction `.dspart` sidecar** — a stored mergeable partial of the bounded reductions. A stored-range downsample of those reductions then **merges the sidecars instead of decoding the value column** (measured **3.2×**), re-keyed to any coarser nesting resolution. A sealed segment is immutable, so the sidecar never goes stale; a rewrite (reconcile/split/squash) regenerates it. | unset (no sidecar; `downsample_range` decodes as before) |
| `DSP_SEGMENT_PARTIAL_MIN_ROWS` | `dsp-server` | Row floor below which a segment gets no partial sidecar (a tiny segment's partial saves too little decode to be worth the extra file). | `4096` |
| `DSP_SEGMENT_PARTIAL_TIERS` | `dsp-server` | Comma-separated fine→coarse rollup resolutions (e.g. `hours,days`) materialized **beside** the `DSP_SEGMENT_PARTIAL_BASE` partial, each re-keyed from the tier below (up to four kept; a finer/non-nesting entry is skipped). A coarse stored-range downsample then folds the coarsest matching tier instead of re-keying the whole fine base (measured **~1.16×** on a DAY query over a MINUTES base). No effect unless `DSP_SEGMENT_PARTIAL_BASE` is set. | unset (base-only sidecar) |
| `RUST_LOG` | all | [`tracing`](https://docs.rs/tracing) filter. On `dsp-server` it drives a per-request root span (`request{method,path,request_id}`, echoed as `x-request-id`) that every per-stage span nests under, each with busy/idle timing: the compute paths (`interpolate.parse`/`compute`/`serialize` under `interpolate.engine`, `downsample.parse`/`reduce`); the **storage read** paths (`storage.{range,value_range,point}.read` + `.serialize`, with a `format` field over JSON/CSV/Arrow/Parquet); the **ingest** paths (`storage.ingest.parse`/`normalize`/`seal` for ILP/CSV/JSON, and `storage.ingest.parquet` for the Parquet decode+seal); and the background **reconcile daemon** (`reconcile.tick{kind,…,aspects,segments}`). | `dsp_tui=debug,database=debug,info` |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | `dsp-server` | When set (e.g. `http://localhost:4317`), export tracing spans to an OpenTelemetry collector over **OTLP/gRPC** in addition to the `RUST_LOG` `fmt` output. Unset → no exporter, no network dependency; a misconfigured/absent collector never blocks startup. `scripts/verify-otlp.sh` verifies delivery end-to-end against a local Jaeger container (starting one if needed). | unset (export disabled) |
| `SKIP_SLOW_TESTS` | tests | Set to `1` to skip long-running tests. | unset |

The database root directory is resolved in this order:

1. `TEST_DATA_DIR`, if set (explicit override for the test suite);
2. otherwise `DSP_DATA_DIR`, if set;
3. otherwise a **portable, per-user default** — `~/.dsp/data` (falling back to the
   platform data directory + `dsp`, e.g. `%APPDATA%\dsp` on Windows, if the home
   directory cannot be determined).

No paths are hard-coded: data lands in a writable, machine-independent location out
of the box, and setting `DSP_DATA_DIR` relocates both the databases and the TUI log
together. The resolution functions are exported as `database::data_dir()` (resolved)
and `database::default_data_dir()` (the raw default).

The `splimes` crate also exposes a build feature:

| Feature | Effect |
|---------|--------|
| `gpu-eager-init` | Initializes the GPU *before* `main()` via a constructor, eliminating first-use latency. |

---

## Library Usage

DSP can be used directly as a set of libraries. The examples below are illustrative —
run `cargo doc --open` for the authoritative, version-matched API.

### Capture Measurements

```rust,ignore
use database::{Database, DatasetId, InputMeasurement, Resolution};
use bigdecimal::BigDecimal;
use std::str::FromStr;
use chrono::{TimeZone, Utc};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let db = Database::new("my_sensors").await?;
    let sensor = db.observe_subject("temperature_sensor_001").await?;
    let temp = db.track_aspect(&sensor.id(), "ambient_temp", &Resolution::Seconds, None).await?;

    let measurements = vec![
        InputMeasurement::new(Utc.with_ymd_and_hms(2024, 1, 1, 12, 0, 0).unwrap(), BigDecimal::from_str("22.5")?),
        InputMeasurement::new(Utc.with_ymd_and_hms(2024, 1, 1, 12, 1, 0).unwrap(), BigDecimal::from_str("22.8")?),
    ];
    db.batch_capture_measurements(temp.id(), DatasetId::new(), measurements).await?;
    Ok(())
}
```

### Query & Interpolate

```rust,ignore
use database::{Database, Resolution, Spline};
use chrono::{TimeZone, Utc};
use futures::StreamExt;

let db = Database::existing("my_sensors").await?;

// Interpolated point at a specific time:
let point = db.analyze_point(&aspect_id, query_time, &Resolution::Seconds, &Spline::Linear).await?;

// Streamed, interpolated range:
let mut stream = db.analyze_range(&aspect_id, start, end, Resolution::Seconds, Spline::Linear).await?;
while let Some(result) = stream.next().await {
    let p = result?;
    println!("{}: {}", p.timestamp, p.value);
}
```

### Run an Analytics Pipeline

```rust,ignore
use database_orchestration::Pipeline;
use database::Database;
use splimes::Spline;

let database = Database::existing("Crypto").await?;
// ... resolve `aspect_id` for the "open" price aspect of BTCUSD ...

let mut pipeline = Pipeline::builder(database.clone(), aspect_id)
    .spline_method(Spline::Linear)
    .batch_size(24)                              // 24-hour batches
    .add_dictionary("daily", "Daily patterns", constraints)
    .with_monthly_increase_detector(0.05)        // 5% monthly increase
    .with_peak_detector("BTC Price Peaks")
    .build()
    .await?;

pipeline.run().await?;                            // full pipeline (incremental)
println!("Signals generated: {}", pipeline.signals().len());
```

For finer control, the individual steps — `prepare_data`, `extract_patterns`,
`detect_events`, `correlate_events`, `generate_signals` — can be called directly.

---

## Terminal UI

`dsp-tui` is the interactive way to explore DSP (the HTTP server is the primary
commercial surface; the TUI serves demos and debugging). Launch it with
`cargo run --release -p dsp-tui`. It is a stateful, keyboard-driven application that
walks you through:

- **Database / Subject / Aspect creation** — with name validation and resolution
  selection (`+`/`-` to step through resolutions from nanoseconds to years).
- **CSV import** — point it at a CSV file and watch live loading progress
  (rows loaded / scanned).
- **Browsing** — select an existing database, subject, and aspect.
- **Plotting** — visualize an aspect's series directly in the terminal.
- **Compression** — choose a mode (time-based, size-based, or combined), configure
  tiers/targets/aggressiveness in a form, and monitor a live compression dashboard
  (phase, tier progress, aggressiveness, compression ratio) plus last-run stats.

Logs are written to `dsp-tui.log` (see `DSP_DATA_DIR`), and the last 50 log lines
are also shown in-app.

---

## Testing & Benchmarks

```bash
# Run the full test suite (use nightly):
cargo test

# Skip long-running tests:
SKIP_SLOW_TESTS=1 cargo test          # PowerShell: $env:SKIP_SLOW_TESTS=1; cargo test

# Test a single crate:
cargo test -p splimes
```

The workspace ships extensive [Criterion](https://docs.rs/criterion) benchmark
suites alongside the DSP-Bench harness (see
[Benchmark Harness](#benchmark-harness)):

| Crate | Benchmarks |
|-------|------------|
| `splimes` | `interpolation`, `gpu_prewarm`, `gpu_cold`, `should_use_gpu_analysis`, `gpu_optimization_bench` |
| `database` | `interpolation_benchmarks`, `integration_interpolation_benchmarks`, `optimized_interpolation_benchmarks`, `strategy_selection_benchmarks`, `new_api_benchmarks`, `cache_performance_benchmarks` |

```bash
cargo bench -p splimes
cargo bench -p database --bench cache_performance_benchmarks
```

HTML reports are generated under `target/criterion/`.

---

## Project Layout

```text
DSP/
├── Cargo.toml                  # workspace manifest (members + shared deps)
├── splimes/                    # spline interpolation engine
│   ├── src/splines/            # CPU spline implementations
│   ├── src/gpu/                # wgpu compute pipeline, shaders, buffer pool
│   ├── src/optimizations/      # CPU / parallel / fast-path strategies
│   └── src/helpers/            # strategy selection, target-time generation
├── database/                   # time-series database (Turso/libSQL control plane + segment store)
│   └── src/types/
│       ├── database/           # connection, config, inputs, outputs, pipeline
│       ├── compression/        # tiered dataset compression
│       ├── batches/            # batching & analysis
│       └── …                   # patterns, events, correlations, signals, …
├── database_orchestration/     # pipeline orchestration & event detectors
├── dsp-physical-type/          # physical encodings, schemas, timestamp codecs, .dspseg
├── dsp-arrow/                  # Arrow/Parquet interchange for segments
├── dsp-arrow-store/            # Arrow/Parquet bridge over the segment store
├── dsp-line-protocol/          # InfluxDB Line Protocol parser (shared dialect)
├── dsp-server/                 # axum HTTP API server
├── dsp-bench/                  # benchmark harness (adapters, profiles, reports)
├── dsp-tui/                    # terminal UI binary
└── legacy/                     # 15 archived predecessor repos (excluded from workspace)
```

The complete work queue and design constraints live in
[`ROADMAP.md`](ROADMAP.md) — the single source of truth.

---

## Performance Notes

- **Batch your writes.** `batch_capture_measurements` uses bulk transactions and is
  dramatically faster than single inserts.
- **Connections are cached.** The database layer reuses connections through a
  TTL/LRU cache (default: 50 connections, 30-minute TTL) and starts MVCC
  `BEGIN CONCURRENT` transactions automatically, with retry + backoff on
  transient failures.
- **Pre-warm the GPU.** Call `splimes::prewarm_gpu()` at startup (or enable
  `gpu-eager-init`) to avoid ~1.2 s of first-call initialization latency.
- **Pick sensible batch sizes** for pipelines (typically 24–100 for hourly data).
- **Let the engine choose.** `auto_interpolate` / `analyze_range` already select the
  optimal backend — overriding is rarely necessary.
- Pipeline pattern loading is **memory-aware**: batch sizes adapt to available RAM to
  avoid exhaustion on large datasets.

---

## Roadmap

[`ROADMAP.md`](ROADMAP.md) is the single source of truth for everything planned,
in progress, and done — a phase-based `[ ]`/`[x]` checkbox queue organized around
the benchmark-led commercial thesis (north star: **dollars per billion
interpolated output points at a p95 latency target**). Shipped capabilities are
documented here as features; every performance claim links to a benchmark
artifact.

---

## License

Released under the [MIT License](LICENSE). Copyright © 2025 Justin Icenhour.
