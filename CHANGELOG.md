# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the project is pre-1.0, minor version bumps may contain breaking changes.

## [Unreleased]

### Changed

- **`splimes` moved to its own repository** ([basic-automation/splimes](https://github.com/basic-automation/splimes))
  and is now a crates.io dependency. It is released on its own schedule, and a stable
  WeftDB waits on a stable splimes.
- **splimes 0.1 → 1.0.** WeftDB depends on `splimes = "1"`, built against the
  `release/1.0` commit through a temporary `[patch.crates-io]` until 1.0.0 is on
  crates.io. splimes' [migration guide](https://github.com/basic-automation/splimes/blob/main/MIGRATING.md)
  lists the results that change; through WeftDB:
  - large inputs keep their method (0.1 swapped cubic for quadratic from 2,500 input
    points and for linear from 5,000);
  - `Polynomial(1 | 2 | 3, b)` stays a polynomial, so it extends outside the data
    instead of holding flat, and with too few points a polynomial steps down to degree
    `n − 1` rather than to `Cubic`/`Quadratic`/`Linear`;
  - a polynomial degree above 8 is an error instead of being capped at 8;
  - a zero-span range returns its one point: `POST /api/v1/interpolate` with a single
    input point and no explicit range answers `200` with that point (it was a `500`);
  - points that coincide with an input return that input's value exactly, and
    interpolated `BigDecimal`s are the shortest decimal that round-trips;
  - time is exact to the nanosecond at every resolution, duplicate timestamps keep the
    last value, and a leap second is the same instant as the start of the next second;
  - `Backend::Auto` uses the GPU only once the program has started it.
- **No interpolation runs on an async worker.** splimes 1.0 is synchronous; `weftdb`'s
  `analyze_point`, `analyze_range` and compression, every `weft-server` interpolation
  endpoint, and the Weft-Bench WeftDB adapter run it on tokio's blocking pool
  (`Interpolator::run_async`, splimes' `tokio` feature). 0.1's `async` functions
  computed on the calling worker.
- **Downsample buckets don't move.** splimes 1.0 removed `Resolution::to_base` and the
  `SECONDS_IN_*` constants; the new `weft_reduce::bucket_index` reproduces 0.1's index
  exactly, including its rounding toward zero before 1970, so bucket edges and the keys
  of persisted `.weftpart` sidecars are unchanged. `Resolution::difference` is replaced
  by `weftdb::units_between`, with the same results.
- `Resolution` names parse case-insensitively (`Hours`, `HOURS`), for example in
  `WEFT_SEGMENT_PARTIAL_BASE`, which ignored anything but lowercase before.
- **`weft-server` provenance comes from splimes.** The `kind` of each interpolated
  point (JSON, CSV, Arrow, Parquet, and the point query) is splimes' `PointKind`, with
  the same `raw` / `interpolated` / `extrapolated` tokens, and `weft_server::PointKind`
  is now a re-export of it. It agrees with the classification `weft-server` did itself
  except at a leap second: an input at `23:59:60.5` and a grid instant at `00:00:00.5`
  the next day are one POSIX instant, so that point is now `raw` (it was `extrapolated`
  or `interpolated`).

### Fixed

- **Commit failures were silently ignored.** A control-plane write that lost an MVCC
  conflict was reported as success. Commit errors now reach the caller, and the
  transaction is rolled back.
- **Quadratic GPU interpolation failed on GPUs without f64 support** (Apple Silicon,
  most integrated GPUs, Windows WARP). The f32 fallback shader did not parse, so wgpu
  panicked.

### Security

- Cleared the `crossbeam-epoch` (RUSTSEC-2026-0204) and `h2` (RUSTSEC-2026-0258)
  advisories, replaced the unmaintained `bincode` (RUSTSEC-2025-0141), and removed the
  unsound `lru` 0.16 (RUSTSEC-2026-0253) by disabling turso's unused full-text search.

## [0.1.0] - 2026-09-25

First public release. WeftDB is pre-beta: the API will change, and the version
number says so.

### Added

- **Interpolation as a first-class query.** Ask for any resolution and get back a
  continuous series, reconstructed with the spline method you choose (linear,
  quadratic, cubic, polynomial), computed on SIMD/parallel CPU or GPU (`wgpu`).
- **Provenance labelling.** Every returned point is marked `raw`, `interpolated` or
  `extrapolated`, so a synthetic value is never silently mistaken for an observed one.
- **Declared precision.** Values are logically `BigDecimal`. Each aspect declares a
  physical encoding (`F64`, `F32`, `ScaledI64`, `ScaledI128`, `Decimal128`,
  `BigDecimalText`) and an error bound; a value the encoding cannot represent within
  that bound is rejected rather than quietly rounded.
- **Typed columnar segment store** (`.weftseg`) on the measurement hot path, with
  bit-packing, delta-of-delta timestamp coding and a transposed value layout, over a
  Turso (libSQL) control plane for catalog, metadata and the segment index.
- **Downsampling** with mergeable partial reductions (`.weftpart` sidecars), including
  time-weighted averages, over an epoch-aligned bucket grid.
- **Apache Arrow interchange** — sealed segments to `RecordBatch` and back, plus Arrow
  IPC and Parquet bytes.
- **HTTP API** (`weft-server`) covering health/readiness, ingest, query, interpolation,
  storage management, backup/restore and a restore drill, with OpenTelemetry tracing.
- **Terminal UI** (`weft-tui`) for exploring and administering an instance.
- **Weft-Bench** (`weft-bench`), a reproducible, correctness-gated benchmark harness.
- **Analytics pipeline** (`weft-orchestration`) chaining batching, pattern extraction,
  event detection, correlation and signal generation.
- Pre-built `weft-server`, `weft-tui` and `weft-bench` binaries for Linux, macOS and
  Windows (x86_64 and arm64) attached to every GitHub release.

### Changed

- **The workspace now builds on stable Rust** (1.95+). The `#![feature(stmt_expr_attributes)]`
  gate is gone, so nightly is no longer required to build, test or depend on any crate.
  Only `cargo fmt` still uses nightly, because `rustfmt.toml` sets nightly-only options.
- **Crates renamed for publication.** The core crate is now `weftdb` (was `database`,
  a name already taken on crates.io) and the pipeline crate is `weft-orchestration`
  (was `database_orchestration`). Import paths change accordingly:
  `use database::…` becomes `use weftdb::…`, and `use database_orchestration::…`
  becomes `use weft_orchestration::…`.

### Internal

- `splimes`' unit-test tree is now gated behind `#[cfg(test)]` instead of compiling
  into released library builds.
- Dropped unused `splimes` dependencies (`flume`, `num_cpus`, `futures`,
  `futures-channel`, `pollster`, and `rand` outside dev builds).

[Unreleased]: https://github.com/basic-automation/weftdb/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/basic-automation/weftdb/releases/tag/v0.1.0
