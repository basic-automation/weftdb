# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the project is pre-1.0, minor version bumps may contain breaking changes.

## [Unreleased]

### Changed

- **`splimes` moved to its own repository** ([basic-automation/splimes](https://github.com/basic-automation/splimes))
  and is now a crates.io dependency (`splimes = "0.1"`). It is released on its own
  schedule, and a stable WeftDB waits on a stable splimes.
- **Turso control plane upgraded 0.6 → 0.8.** ⚠️ This is one-way: once 0.8 writes a
  store, its MVCC log is v3 and an older WeftDB can no longer open it. Back up the
  control plane before upgrading.
- **`.weftpart` sidecars use `postcard` instead of `bincode`** (frame v3). Sidecars
  written by older versions are ignored: the segment is decoded instead, and the
  sidecar is rebuilt on the next seal. No data migration is needed.
- All dependencies updated to their latest major versions, including wgpu 30,
  Arrow/Parquet 60 and OpenTelemetry 0.33.
- Builds on stable Rust (MSRV 1.95); nightly is no longer required.

### Added

- `Database::list_stored_databases()` lists the legacy databases on disk (the folders
  of the data directory that hold a `metadata.db`), sweeping the build directories of
  interrupted `Database::new` calls first. `weft-tui` lists databases through it.
- `Outputs::get_unbatched_entries` and `Inputs::dequeue_unbatched_entries` read and
  dequeue unbatched-queue entries together with their `queued_at` (`UnbatchedEntry`),
  so a queue consumer dequeues only what it read; see the ingest fix below.
- `Inputs::remove_extracted_batches` removes the processed batches pattern extraction
  consumed and records their hashes, so the batch consumer never queues them again;
  `build_patterns_queue` uses it (see the batch dedupe fix below).
- `Database::get_earliest_measurement_uncached` reads an aspect's earliest measurement
  from its rows, bypassing the per-instance cache; the incremental build aligns its
  windows on it.
- `weft-orchestration` has a `fault-injection` feature (it enables weftdb's) for its
  crash tests: `cargo test -p weft-orchestration --features fault-injection --test legacy_queue_crash`.

### Fixed

- **`Database::new` could leave a half-created database** that neither a retry of
  `new` ("already exists") nor `Database::existing` (no `database` row) could use. A
  database is now built in a hidden `.{name}.creating-{nonce}` folder beside its final
  place and renamed to `{name}` only once its schema and `database` row are committed,
  then the data directory is fsynced. An error or crash leaves either nothing under
  `{name}` (retry `new`) or the complete database (open it with `existing`). Build
  folders left by a crash are removed by the next `new` or listing; the data directory
  gains a `.weft-creating.lock` file that keeps a sweep away from a creation in
  progress, in this or another process. `new` now refuses a name of the build folders'
  form (`.{x}.creating-` followed by 32 hex digits). If the published database cannot
  be opened at its final path, the error says it was created and to open it with
  `existing`; the checkpoint after creation is best-effort.
- **Legacy ingest could store rows that were never batched.** `batch_capture_measurements`
  and `capture_measurement` queued the timestamps for batching only after the rows
  committed, so a failure in between left rows the incremental pipeline never saw.
  The timestamps are now queued before the insert, and queued again once their rows (or
  each chunk) are committed. The incremental build no longer fails on an aspect whose
  queue holds timestamps but no rows yet.
- **An incremental build running during an ingest could drop the ingest's rows.** It
  could read a queued timestamp before its row was committed, build the window without
  it and dequeue it, and it cleared the whole queue when every queued timestamp lay
  before the earliest stored row, as a backfill's do. Enqueuing a timestamp that is
  already queued now moves its `queued_at` forward (it used to keep the old one), and
  the build dequeues only the entries it read with the `queued_at` it read, so the
  second enqueue after the commit keeps the row queued for the next build. The build
  now reads the earliest measurement, which it aligns its windows on, from the rows on
  every run: a cached value (kept per `Database` instance for up to 10 minutes) made it
  clear a backfill from the queue. Ingest also drops that cached value after each
  commit, but only in the instance (and its clones) that ingests; other instances, such
  as the one each weft-tui action opens, keep theirs until it expires. A window the
  build batched from rows that were already committed is rebuilt identically after the
  second enqueue, and the batch dedupe skips it. Residuals, for a build running while
  ingesting into the same aspect: the ingest's rows stay unbatched if the ingest dies
  (or exhausts its retries) between a commit and its second enqueue; and a build run
  between two chunks of a backfill or gap fill larger than one chunk batches the
  windows spanning committed rows and rows still to come with the latter interpolated,
  so those windows get a second, different batch and occurrence once the rows land.
- **Concurrent writes to the batching queue failed the call.** The queue enqueue is an
  upsert, so it writes a timestamp that is already queued, and an MVCC write-write
  conflict with a consumer's dequeue, or with another ingest of the same timestamps,
  failed the ingest (before its rows were stored) or the consumer's dequeue. Ingest's
  enqueues and the consumer's dequeue now retry such a conflict, for about three
  seconds; a single `enqueue_unbatched_measurements` call returns it as
  `Error::TransientMvccError`. The consumer dequeues in transactions of at most 500
  entries, so an enqueue never waits long for it.
- **Legacy ingest could fail after its rows were stored.** The steps after the commit
  (the second enqueue, the checkpoint, the dirty-region marking, the aspect's
  earliest/latest columns and `capture_measurement`'s transaction-log entry) are now
  best-effort: a failure is logged and the call returns `Ok`, since a client would retry
  an error into duplicate rows.
- **The batch consumer could queue a batch twice.** It rebuilds every window a queued
  timestamp falls in, so after a crash before its dequeue, or once an ingest that ran
  during it queued its committed rows again, it rebuilt windows it had already batched,
  and each became a second pattern occurrence. `insert_unprocessed_batch` and
  `batch_insert_unprocessed_batches` now skip a batch whose `(aspect_id, batch_hash)` is
  already queued, processed, or extracted. Processed batches keep the hash they were
  queued under (it used to be recomputed from the processed measurements), and pattern
  extraction records the hashes of the batches it consumes in a new `extracted_batches`
  table of the processed batch database, in the transaction that deletes them: one
  small row per extracted batch, removed by `clear_processed_batches` (which
  `Pipeline::prepare_data_full_rebuild` calls), so a full rebuild still extracts every
  window again. A first-run full rebuild (`is_first_run`) now skips the windows already
  extracted. The unprocessed and processed batch databases gain an index,
  `idx_batches_hash` on `batches(aspect_id, batch_hash)`, and `extracted_batches` one on
  `(aspect_id, batch_hash)`, created when a process first opens them, so each check is
  a point lookup.
- Rows-mode ingest residuals remain until aspects move to the segment store, and are now
  documented on `batch_capture_measurements` and `capture_measurement`: an error mid-call
  leaves the chunks committed before it, and retrying stores those rows again.
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
