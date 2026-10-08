# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the project is pre-1.0, minor version bumps may contain breaking changes.

## [Unreleased]

### Added

- **Interpolation grids requested over HTTP are capped** at 10,000,000 points per
  request by default (`weft_server::MAX_INTERPOLATE_OUTPUT_POINTS`), on every
  `/api/v1/interpolate*` endpoint; set `WEFT_MAX_INTERPOLATE_POINTS` to change it. The
  variable is read once at startup, and a value that is not a positive integer stops
  `weft-server` from starting. A larger grid is a `400` that names its size and the
  limit, refused before anything is allocated; before, a fine resolution over a long
  range tried to allocate all of it.
- **`weft-server` calibrates the interpolation backends at startup.** Once the listener
  is bound, it runs `splimes::calibrate()` once in the background on a blocking thread:
  that starts the GPU if there is one, times the single-thread, rayon and GPU backends,
  and sets where `Backend::Auto` switches between them. It takes several seconds and
  never delays serving or fails startup; until it finishes, requests interpolate on the
  CPU with splimes' default thresholds. The adapter (or why there is none) and the
  thresholds are logged. A CPU/software adapter (llvmpipe, lavapipe, WARP) is not
  calibrated unless `WEFT_GPU_CALIBRATE=force`; `WEFT_GPU_CALIBRATE=0` skips calibration.
  Without it, splimes 1.0 never uses the GPU.
- **Weft-Bench calibrates like `weft-server`.** An interpolation run (line protocol or
  `--synthetic`) calls `splimes::calibrate()` once before anything is timed, skipping a
  CPU/software adapter as the server does (it has no counterpart to
  `WEFT_GPU_CALIBRATE=force`), so the numbers come from the engine the server runs on
  that machine; `--no-gpu-calibrate` turns it off. The calibration, the GPU and
  `Backend::Auto`'s thresholds are printed and recorded in the report's
  `metadata.engine` (bench schema v16) and its HTML view.
- **Third-party attribution.** A root `NOTICE`, and a `THIRD-PARTY-NOTICES` file shipped in
  the `weft-physical-type` and `weft-reduce` packages, credit the two Apache-2.0 projects
  whose code is adapted here: the Chimp/Chimp128 codecs (from the authors' reference
  implementation) and the DDSketch quantile sketch (from Datadog's sketches-java), with the
  upstream NOTICE text and the license. The Chimp128 docs no longer credit DuckDB as the
  source.

### Changed

- **`splimes` moved to its own repository** ([basic-automation/splimes](https://github.com/basic-automation/splimes))
  and is now a crates.io dependency. It is released on its own schedule, and a stable
  WeftDB waits on a stable splimes.
- **splimes 0.1 → 1.0.** WeftDB depends on `splimes = "1"`, resolved to 1.0.0 from
  crates.io. splimes' [migration guide](https://github.com/basic-automation/splimes/blob/main/MIGRATING.md)
  lists the results that change; through WeftDB:
  - large inputs keep their method (0.1 swapped cubic for quadratic from 2,500 input
    points, and cubic and quadratic for linear above 5,000);
  - `Polynomial(1 | 2 | 3, b)` stays a polynomial, so it extends outside the data
    instead of holding flat, and with too few points a polynomial steps down to degree
    `n − 1`. 0.1 stepped down to `Cubic`/`Quadratic`/`Linear` with four input points or
    fewer, and with five or more kept the requested degree and failed;
  - a polynomial degree above 8 is an error instead of being capped at 8;
  - a zero-span range returns its one point: `POST /api/v1/interpolate` with a single
    input point and no explicit range answers `200` with that point (it was a `500`);
  - points that coincide with an input return that input's value exactly, and
    interpolated `BigDecimal`s are the shortest decimal that round-trips;
  - time is exact to the nanosecond at every resolution, duplicate timestamps keep the
    last value, and a leap second is the same instant as the start of the next second;
  - results no longer depend on whether the GPU ran them. With a GPU present, 0.1's
    `auto_interpolate` (behind `analyze_range`, `analyze_point`, compression and every
    `weft-server` interpolation endpoint) ran a call on it when the larger of its input
    count and grid size was 100–999, or either reached 50,000. Its GPU path evaluated a
    polynomial of degree 0 or 4–7 one degree higher than the CPU did (both capped at
    `n − 1` and 8) and, in `f64`, ignored any bounds factor other than 1.0, so those
    calls returned different polynomial values from the CPU's, and on a GPU without
    `f64` support it computed in `f32`. 1.0 computes the same method, in `f64`, on every
    backend;
  - `Backend::Auto` uses the GPU only once the program has started it, and only above
    the calibrated thresholds (see the startup calibration above); without calibration
    every interpolation runs on the CPU.
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
- **Breaking for Rust users: `weftdb::Resolution` and `weftdb::Spline` are
  `#[non_exhaustive]`.** They are re-exports of splimes' types, which 1.0 marks
  non-exhaustive so new variants can arrive in minor releases: an exhaustive `match` on
  either needs a `_` arm. Their removed 0.1 helpers go with them: use
  `Resolution::step()`/`step_nanos()` for `to_step()` and the `SECONDS_IN_*` constants,
  `weftdb::units_between` for `difference`, `weft_reduce::bucket_index` for `to_base`,
  and `Spline::min_points()` for `number_of_points_required()`. `Resolution::round()`
  and `to_step_base()` need no replacement: a step is one unit at every resolution, so
  `to_step_base()` was always `1` and `round()` returned its input unchanged (or, at
  nanosecond resolution outside 1677–2262, an error). `Spline::pre_check()` has no
  direct replacement: `Spline::validate()` checks a polynomial's parameters, and the
  engine checks the range and steps down when there are too few points (or, under
  `Interpolator::exact(true)`, returns `InsufficientPoints`).
- **Stored spline methods are validated when they are loaded.** A dictionary created
  with `AspectStructure::new_dictionary` persists its step interpolation as the
  `Spline`'s text (`steps_interpolation` in the `dictionary_constraints` table of
  `<aspect>/dictionaries/<dictionary>.db`); a dictionary that a pipeline registers
  through `weft_orchestration::load_dictionary` stores no constraints and is not
  affected. splimes 1.0's `Spline::from_str` validates what it parses, so a stored
  `Polynomial` with degree 0, a degree above 8, or a negative or non-finite bounds
  factor, which 0.1 accepted, no longer loads: `get_dictionary_metadata` returns
  `Database error: Invalid interpolation format: invalid polynomial degree 9: must be
  between 1 and 8` (or `… invalid bounds factor -1: must be finite and not negative`),
  and `load_dictionary` logs that error as a warning ("Failed to check dictionary
  metadata, attempting to create"). Until this release `get_dictionary_metadata` could
  not read any stored dictionary (see Fixed), so this is the first release that reads,
  and so checks, a stored method at all. To fix it, stop WeftDB and rewrite the stored
  value with Turso's shell (`tursodb`, not `sqlite3`: the file is in Turso's MVCC
  journal mode), e.g. `UPDATE dictionary_constraints SET steps_interpolation =
  'Polynomial(degree: 8, bounds_factor: None)' WHERE steps_interpolation =
  'Polynomial(degree: 9, bounds_factor: None)'`; 0.1 capped any degree above 8 at 8.
  Degree 0 and invalid bounds factors have no exact 1.0 equivalent: choose a degree
  from 1 to 8 and a finite, non-negative bounds factor, or `None`. `new_dictionary`
  now checks the method with `Spline::validate()` and refuses one that would not load,
  e.g. `Invalid step interpolation for dictionary 'd': invalid polynomial degree 9: must
  be between 1 and 8`; it stored any method before.
- `Resolution` names parse case-insensitively (`Hours`, `HOURS`), for example in
  `WEFT_SEGMENT_PARTIAL_BASE`, which ignored anything but lowercase before.
- **`weft-server` provenance comes from splimes.** The `kind` of each interpolated
  point (JSON, CSV, Arrow, Parquet, and the point query) is splimes' `PointKind`, with
  the same `raw` / `interpolated` / `extrapolated` tokens, and `weft_server::PointKind`
  is now a re-export of it. It agrees with the classification `weft-server` did itself
  except at a leap second: an input at `23:59:60.5` and a grid instant at `00:00:00.5`
  the next day are one POSIX instant, so that point is now `raw` (it was `extrapolated`
  or `interpolated`).
- **Invalid polynomial parameters are a `400`.** A `polynomial` spline with degree 0, a
  degree above 8, or a negative `bounds_factor` used to be accepted, and the
  interpolation endpoints usually answered `200` with a result (0.1 capped a degree
  above 8 at 8). Not always: with 5 to `degree` input points a degree above 8 was a
  `500` (0.1's failed step-down, above), and on a machine without a usable GPU, a
  request whose larger of input count and grid size was 100–999 or 50,000 and up ran
  0.1's SIMD path, where degree 0 was a `500` and a bounds factor below −0.5 panicked
  once it extrapolated non-constant data, dropping the connection without a response.
  splimes 1.0 rejects them, and the request is now a `400` naming the problem, e.g.
  `invalid polynomial degree 9: must be between 1 and 8`. (JSON cannot carry a
  non-finite bounds factor; such a body is a `400` from the JSON parser before splimes
  sees it.)
- **Interpolation errors map to HTTP status by kind.** An input or extrapolated value
  beyond `f64`'s range and an oversized grid are `400`s too; any other engine error
  (a GPU failure, a panicked blocking task) stays a `500`, as every engine error was
  before.
- **Turso control plane upgraded 0.6 → 0.8.** ⚠️ This is one-way: once 0.8 writes a
  store, its MVCC log is v3 and an older WeftDB can no longer open it. Back up the
  control plane before upgrading.
- **`.weftpart` sidecars use `postcard` instead of `bincode`** (frame v3). Sidecars
  written by older versions are ignored: the segment is decoded instead, and the
  sidecar is rebuilt on the next seal. No data migration is needed.
- `weft-bench --spline poly:N` rejects a degree outside 1–8 when the arguments are
  parsed, naming the limit. 0.1.0 accepted any degree, and with splimes 1.0 such a run
  would only have failed once the engine rejected it.
- All dependencies updated to their latest major versions, including wgpu 30,
  Arrow/Parquet 60 and OpenTelemetry 0.33.
- Builds on stable Rust (MSRV 1.95); nightly is no longer required.
- **Advisory codecs moved behind the `experimental-codecs` feature** (`weft-physical-type`).
  ⚠️ Breaking for code that calls them: the `floatcodec` module (Gorilla-XOR, Chimp,
  Chimp128, Elf and `best_f64_*`), `ColumnEncoding::{gorilla_f64_bytes, best_f64_bytes,
  best_f64_codec}` and the FIRE forecaster (`fire_*`) now need
  `features = ["experimental-codecs"]`. None of them was ever written to disk, so stored
  segments are unaffected. The feature is off by default, outside the semver promise, and
  pending patent review.
- **The opt-in transposed value codec is now the `bitsliced-codec` feature**
  (`weft-physical-type`, forwarded by `weftdb` and `weft-server`), pending patent review.
  ⚠️ A store that enabled it with `WEFT_SEGMENT_TRANSPOSED_MAX_OVERHEAD` must be built with
  `bitsliced-codec` to read those segments: without the feature, reading one fails with the
  new `WeftSegError::CodecNotEnabled` (which names the feature) instead of decoding, and the
  variable is ignored with a warning. The `transpose_bitpack_*` primitives, `TRANSPOSE_TILE`,
  `ColumnEncoding::{transposed_value_bytes, transposed_overhead, best_value_codec_transposed}`
  and `weftseg::write_value_column_transposed` need the feature too. Default builds never
  wrote this codec, so their stores are unaffected. The docs now call it a bit-sliced
  (bit-plane-major) layout; it is not the FastLanes layout they used to name.

### Fixed

- **Interpolation responses report the method that ran.** `spline` in the
  `/api/v1/interpolate` and `/api/v1/interpolate/ilp` JSON responses and in the point
  query is documented as the method actually used, but echoed the requested one. With
  fewer distinct timestamps than a method needs, splimes steps down (`Cubic` →
  `Quadratic` → `Linear`, `Polynomial(d, b)` → `Polynomial(n − 1, b)`), so a `cubic`
  request over three points ran a quadratic and still said `Cubic`; it now says
  `Quadratic`. The CSV, Arrow and Parquet outputs carry no method field. The
  `interpolate.engine` trace span keeps the requested method in `spline` and now
  records the one that ran as `effective_spline`.
- **`get_dictionary_metadata` and `list_dictionaries` failed for every stored
  dictionary.** Both selected an `updated_at` column that the `dictionary_metadata`
  table does not have (`Failed to query dictionary metadata: … no such column:
  updated_at`), and `get_dictionary_metadata` then read `steps_count`, an `INTEGER`
  column, as text. They read the table as it is now: the count as an integer (text is
  still accepted), and a dictionary without steps as `None`. The metadata
  `get_dictionary_metadata` caches is keyed by aspect as well as name, so two aspects'
  dictionaries of the same name no longer share it.
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
