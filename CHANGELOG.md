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
- **A segment store root can be open in only one `SegmentStore` at a time.** ⚠️ A
  second `SegmentStore::open`/`open_scoped` on a root that this process already has
  open used to succeed, so several scopes could share one root at once. It now fails
  with the new `StoreLocked` error (downcast it from the `anyhow::Error`). Embedders
  that kept several scoped stores open on one root must close one before opening the
  next, or give each scope its own root.
- **Control-plane backups are published atomically, with a manifest.** A backup is
  built in a `.partial-{label}-{nonce}/` directory beside its destination, each
  database verified and fsynced, then a `MANIFEST.json` (format, creation time, and
  each file's size, tables and rows) is written and fsynced, the directory fsynced,
  renamed to its label and the parent fsynced. A backup directory now holds the four
  databases plus `MANIFEST.json`. Backing up into a directory that already exists is
  refused (it was accepted when empty). A backup that fails removes its build
  directory again, unless it fails after the rename (only the parent's fsync is left
  then): that error, a `500` over HTTP, leaves the complete backup under its label.
- **Backup retention counts only complete backups**: a `backup-<digits>` directory
  with a `MANIFEST.json`, or one from before manifests that holds all four databases.
  Pruning renames a backup to `.deleting-*` (durably) before removing its files. A
  backup directory that cannot be inspected (a permission error, say) is skipped and
  logged instead of failing retention. The backup daemon removes `.partial-*`,
  `.deleting-*` and `.restore-drill-*` entries left untouched for an hour, at start
  and on every tick; without the daemon, what a crash leaves there stays until
  removed by hand.
- **Backup and drill labels starting with `.partial-`, `.deleting-` or
  `.restore-drill-` are rejected with `400`**: those names belong to unfinished
  backups, prunes and drills, which are never restored and are swept.
- **`restore_control_plane` stages each file as `<name>.tmp`**, synced and verified,
  renames them into place only once all four verify, and fsyncs the root, which it
  now creates durably. A backup with a manifest must match it (sizes, tables, rows).
- Library API: `verify_snapshot` and `snapshot_with_verify` take the tables the
  snapshot must hold, and `SnapshotReport` lists them in `table_names`.
  `sweep_backup_staging_at` is `sweep_backup_staging` with an explicit clock. `StoreFs`
  gains `create_dir`, `remove_dir_all` and `copy_new`, and the `SimFs` power-cut
  simulator (`fault-injection` feature) now models directories.

### Fixed

- **A segment store root is now owned by one process.** Opening a store takes a `LOCK`
  file in the root (with the holder's pid and session in `LOCK.holder` beside it) and
  holds it until the store closes. A second `weft-server` on the same
  `WEFT_SEGMENT_STORE_ROOT` now exits at startup with an error naming the pid that
  holds the root, instead of Turso's "File is locked by another process". The lock is
  released by the OS however the holder exits, so there is never a stale lock to
  clear. (In-process, see the `StoreLocked` entry under Changed.)
- **A control-plane database that could not switch to MVCC was opened anyway.** The
  segment store's four control-plane databases now refuse to open unless they run in
  MVCC journal mode and a new connection syncs FULL, which is what makes a COMMIT
  durable when it returns. Previously a failed switch was ignored and the database
  committed in WAL mode.
- **Opening a store now makes its files' directory entries durable.** The root,
  `segments/`, the root's parent and any directory the open created are fsynced, so
  the control-plane databases and their logs cannot vanish from the directory after a
  power cut. A pre-existing root whose parent the server cannot read still opens, with
  a warning.
- **The store's database/subject registration is one transaction.** A crash during
  open could leave the database registered without its subject.
- **A backup that stopped part-way could count as a backup.** A crash or error
  mid-backup left a `backup-<digits>` directory with only some of its databases,
  which retention counted (so it could prune a good backup in its place) and a drill
  could pick. A pruned backup interrupted part-way was left half-removed under its
  name. Neither can happen now; see above.
- **A reported backup could be lost on power loss**: nothing fsynced its files'
  directory entries or the directory itself.
- **An empty control-plane snapshot passed verification.** Every snapshot must now
  hold its database's tables, at backup and at restore.
- **The restore drill discarded a failed cleanup.** `POST /api/v1/storage/restore/drill`
  now reports it in a new `cleanup_error` field (`null` when the rehearsal copy was
  removed).
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
