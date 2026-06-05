# DSP

**A high-performance, GPU-accelerated time-series database and analytics platform written in Rust.**

![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)
![Rust](https://img.shields.io/badge/Rust-nightly-orange.svg)
![Platform](https://img.shields.io/badge/platform-Windows%20%7C%20macOS%20%7C%20Linux-blue.svg)
![Status](https://img.shields.io/badge/status-active%20development-green.svg)

DSP ingests raw, irregularly-sampled time-series measurements (sensor readings,
financial prices, telemetry, …), stores them with arbitrary-precision values,
and turns them into insight: it **interpolates** data to any resolution using
GPU/CPU/SIMD spline engines, **extracts recurring patterns**, **detects events**,
**correlates** the two, and **generates prediction signals** — all driven from an
interactive terminal UI or used directly as a set of Rust libraries.

---

## Table of Contents

- [Overview](#overview)
- [Key Features](#key-features)
- [Architecture](#architecture)
- [Data Model](#data-model)
- [The Analytics Pipeline](#the-analytics-pipeline)
- [Interpolation Engine](#interpolation-engine)
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

DSP is organized as a Rust **Cargo workspace** with four crates that build on one
another, from a low-level numerical engine up to an interactive application:

| Crate | Role |
|-------|------|
| [`splimes`](splimes) | Spline interpolation engine — Linear / Quadratic / Cubic / Polynomial methods with automatic GPU, parallel, SIMD, and CPU strategy selection. |
| [`database`](database) | Time-series database built on [Turso](https://turso.tech/) (libSQL) with MVCC concurrent writes, a hierarchical data model, pattern-recognition types, and tiered dataset compression. |
| [`database_orchestration`](database_orchestration) | High-level pipeline that chains batching → pattern extraction → event detection → correlation → signal generation, with built-in detectors and parallel execution. |
| [`dsp-tui`](dsp-tui) | Terminal user interface (Ratatui + Crossterm) for creating databases, importing CSV data, browsing and plotting aspects, and running compression. |

The platform is designed for **real-time and large-scale** workloads: measurements
are stored with [`BigDecimal`](https://docs.rs/bigdecimal) precision, interpolation
transparently scales from a handful of points to millions across the GPU, and the
storage layer uses bulk transactions and intelligent caching throughout.

---

## Key Features

- **Arbitrary-precision time-series storage** — values are `BigDecimal`, timestamps
  are UTC, and resolution can be anything from **nanoseconds to years**.
- **Hierarchical data model** — `Database → Subject → Aspect → Measurement`, with
  each aspect getting its own isolated set of SQLite/libSQL databases on disk.
- **Adaptive interpolation** — the engine automatically picks GPU, parallel (Rayon),
  SIMD (`wide`), or single-threaded CPU execution based on dataset size, with
  graceful method fallback (Cubic → Quadratic → Linear) when data is sparse.
- **GPU acceleration** — compute shaders (WGSL) run over [`wgpu`](https://wgpu.rs/)
  on Vulkan, Metal, or DX12, with buffer pooling, persistent staging buffers, and
  f64/f32 precision paths.
- **Pattern recognition & prediction** — extract recurring shapes into dictionaries,
  detect events (peaks, valleys, threshold crossings, monthly increases, drawdowns),
  correlate patterns with events, and emit probability-weighted prediction signals.
- **Tiered dataset compression** — shrink historical data by time (older = more
  compressed) and/or to a target size, with progress reporting and dirty-region
  re-compression.
- **MVCC concurrency** — concurrent writes via `BEGIN CONCURRENT` transactions.
- **Interactive TUI** — drive the whole platform from the terminal, including live
  CSV import progress, plotting, and compression dashboards.

---

## Architecture

```text
┌─────────────────────────────────────────────────────────────────────┐
│                              dsp-tui                                 │
│         Ratatui + Crossterm terminal UI (the application)           │
│   create DB · import CSV · browse subjects/aspects · plot · compress │
└───────────────────────────────┬─────────────────────────────────────┘
                                │ uses
                ┌───────────────┴───────────────┐
                ▼                                ▼
┌──────────────────────────────┐   ┌────────────────────────────────────┐
│    database_orchestration    │   │              database              │
│  Pipeline orchestration:     │──▶│  Turso/libSQL time-series store     │
│  batch → patterns → events   │   │  Subject/Aspect/Measurement model   │
│  → correlations → signals    │   │  Patterns · Events · Compression    │
│  Built-in event detectors    │   │                                    │
└───────────────┬──────────────┘   └──────────────────┬─────────────────┘
                │ both depend on                       │ depends on
                └──────────────────┬───────────────────┘
                                   ▼
                  ┌──────────────────────────────────┐
                  │             splimes              │
                  │   Spline interpolation engine    │
                  │  GPU (wgpu/WGSL) · Rayon · SIMD  │
                  │  Automatic strategy selection    │
                  └──────────────────────────────────┘
```

Dependencies flow downward: `dsp-tui` and `database_orchestration` depend on
`database`, and everything ultimately depends on `splimes` for numerical work.

---

## Data Model

DSP organizes data hierarchically. Each level maps to a directory or database file
on disk, so an aspect's storage is fully self-contained.

| Concept | Description | Example |
|---------|-------------|---------|
| **Database** | Top-level container for all data. | `Crypto`, `my_sensors` |
| **Subject** | A logical entity being observed. | `BTCUSD`, `temperature_sensor_001` |
| **Aspect** | A specific measurement type of a subject, with its own resolution and (optional) compression config. | `open`, `ambient_temp` |
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
methods, choosing the fastest execution strategy automatically.

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

- **Buffer pooling** — size-tiered, LRU-evicted pool that cuts per-interpolation
  allocations dramatically and reuses buffers across batches.
- **Persistent staging buffers** — round-robin buffers that eliminate unmap/remap
  overhead.
- **f64 / f32 precision paths** — automatically chosen by GPU capability.
- **Pre-warming** — `splimes::prewarm_gpu()` (or the `gpu-eager-init` feature)
  removes first-call initialization latency.

```rust,ignore
use splimes::{auto_interpolate, Resolution, Spline};

// Automatically selects GPU/CPU/parallel based on size:
let series = auto_interpolate(&mut points, start, end, Resolution::Seconds, Spline::Cubic).await?;
```

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
| `RUST_LOG` | all | [`tracing`](https://docs.rs/tracing) filter. | `dsp_tui=debug,database=debug,info` |
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

`dsp-tui` is the primary way to interact with DSP. Launch it with
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
suites:

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
├── database/                   # time-series database (Turso / libSQL)
│   └── src/types/
│       ├── database/           # connection, config, inputs, outputs, pipeline
│       ├── compression/        # tiered dataset compression
│       ├── batches/            # batching & analysis
│       └── …                   # patterns, events, correlations, signals, …
├── database_orchestration/     # pipeline orchestration & event detectors
│   └── src/
│       ├── pipeline.rs         # Pipeline + builder API
│       ├── detectors.rs        # built-in event detectors
│       └── batch_utils/        # batch queue helpers
└── dsp-tui/                    # terminal UI binary
    └── src/                    # app state machine, rendering, logging
```

Additional design notes live in the repo root and crate directories:
[`IMPLEMENTATION_SUMMARY.md`](IMPLEMENTATION_SUMMARY.md),
[`GPU_OPTIMIZATION_COMPLETE.md`](GPU_OPTIMIZATION_COMPLETE.md),
[`OPTIMIZATION_ANALYSIS.md`](OPTIMIZATION_ANALYSIS.md), and
[`PHASE5_ROADMAP.md`](PHASE5_ROADMAP.md).

---

## Performance Notes

- **Batch your writes.** `batch_capture_measurements` uses bulk transactions and is
  dramatically faster than single inserts.
- **Pre-warm the GPU.** Call `splimes::prewarm_gpu()` at startup (or enable
  `gpu-eager-init`) to avoid ~1.2 s of first-call initialization latency.
- **Pick sensible batch sizes** for pipelines (typically 24–100 for hourly data).
- **Let the engine choose.** `auto_interpolate` / `analyze_range` already select the
  optimal backend — overriding is rarely necessary.
- Pipeline pattern loading is **memory-aware**: batch sizes adapt to available RAM to
  avoid exhaustion on large datasets.

---

## Roadmap

GPU optimization has completed Phases 1–4 (buffer pooling, persistent staging
buffers, async handles, configuration API). Planned future work
(see [`PHASE5_ROADMAP.md`](PHASE5_ROADMAP.md)):

- **Phase 5** — GPU command batching.
- **Phase 5.5** — true async GPU I/O for CPU–GPU overlap.
- **Phase 6** — memory-mapped buffers for lower latency.
- **Phase 7** — multi-GPU scaling.

---

## License

Released under the [MIT License](LICENSE). Copyright © 2025 Justin Icenhour.
