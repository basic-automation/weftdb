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
- **Breaking for implementors of `Inputs` and `Outputs`.** Three required methods were
  added without default implementations: `Inputs::dequeue_unbatched_entries`,
  `Inputs::remove_extracted_batches` and `Outputs::get_unbatched_entries` (see Added).
  Behaviour that implementations must now match: `insert_unprocessed_batch` and
  `batch_insert_unprocessed_batches` return `Ok` when they skip a batch that is already
  queued, processed or extracted; a processed batch's `batch_hash` is the hash it was
  queued under, not a hash of its processed measurements; and
  `enqueue_unbatched_measurements` is an upsert that moves an already-queued entry's
  `queued_at` forward and reports a write-write conflict as
  `Error::TransientMvccError`.
- **Rows-mode ingest and the incremental build commit more often** (crash-consistency
  fixes below). `capture_measurement` makes four synced commits where it made three
  (the write-ahead enqueue, the row, the second enqueue, the transaction-log entry), and
  `batch_capture_measurements` one more per chunk (each chunk's second enqueue). The
  incremental build dequeues in transactions of at most 5,000 entries, one synced
  commit each, where a run dequeued in a single commit, so a backfill of a million rows
  costs about 200 dequeue commits.
- `weft-tui` lists databases with `Database::list_stored_databases`: the names are
  sorted, and an unreadable data directory is reported as an error instead of showing
  an empty list (a missing one still lists nothing).
- **A segment store's `segment_index.db` is migrated to store layout 2 when it opens**
  (crash-consistency design, S6). The migration is additive and idempotent and
  rewrites no data: `segment_index` gains the columns `gen` (0 on every existing row),
  `prec`, `frame_crc` and `commit_epoch`, and the database gains the tables
  `aspect_seq`, `frame_journal`, `ingest_ledger`, `segment_quarantine`, `store_meta`,
  `aspect_metadata` and `segment_changes`, which later releases fill. `store_meta`
  records `layout_version = 2` and a `store_uuid`. An earlier WeftDB still opens a
  migrated store, which ignores the additions; a WeftDB refuses a store whose
  `layout_version` is newer than it knows, before it changes anything in it.
- Segment-index writes (seals, reconciles, splits, merges, squashes) commit through
  one transaction type. A write that loses an MVCC conflict to another writer of the
  same row still fails the call at once, as before: retrying it would replay it over
  the other writer's committed row. Only a database that is busy before the
  transaction starts is retried, up to five times with backoff.

### Added

- **Write poison** (crash-consistency design, S6). When a segment-index COMMIT fails
  in a way that may still have committed (any COMMIT error except a conflict Turso
  detects while validating the transaction, which it rolls back before writing
  anything), the transaction may or may not be durable, so the store now refuses every
  write (seal, declare, reconcile, split, merge, squash, compact, rollup rebuild) with
  the new `weftdb::Poisoned` error until the process restarts, while reads keep
  working; the restart's open settles the transaction. `SegmentStore::poisoned()`
  reports it, and `GET /ready` gains `poisoned`, `restart_required` and
  `poison_reason`. `WEFT_ON_AMBIGUOUS_COMMIT=exit` makes the process log and exit with
  status 70 instead, for deployments whose supervisor restarts it (unset or `poison`
  keeps the default).

- `Database::list_stored_databases()` lists the legacy databases on disk (the folders
  of the data directory that hold a `metadata.db`), sweeping the build directories of
  interrupted `Database::new` calls first. `weft-tui` lists databases through it.
- `Outputs::get_unbatched_entries` and `Inputs::dequeue_unbatched_entries` read and
  dequeue unbatched-queue entries together with their `queued_at` (`UnbatchedEntry`),
  so a queue consumer dequeues only what it read; see the ingest fix below.
- `Inputs::remove_extracted_batches` removes the processed batches pattern extraction
  consumed and records their hashes, so the batch consumer does not queue them again
  while the record keeps them; `build_patterns_queue` uses it (see the batch dedupe fix
  below).
- `WEFT_EXTRACTED_BATCH_RETENTION_SECS` (weftdb): how long, in seconds, the record of
  extracted batches keeps a batch's hash. A positive whole number; anything else keeps
  the default, 48 hours. Set it above the longest interval between pipeline runs of an
  aspect plus the longest ingest call (see the batch dedupe fix below).
- `Database::get_earliest_measurement_uncached` reads an aspect's earliest measurement
  from its rows, bypassing the per-instance cache; the incremental build aligns its
  windows on it.
- `weft-orchestration` has a `fault-injection` feature (it enables weftdb's) for its
  crash tests: `cargo test -p weft-orchestration --features fault-injection --test legacy_queue_crash`.

### Fixed

- **A segment store that was moved, restored into another root or mounted at another
  path could not read its frames.** The index records each frame by the absolute path
  it was written under, and every read opened that path. Reads now resolve it against
  the store's current root: a path under the root is used as it is, and any other is
  taken as `root/segments/` plus what follows its last `segments` component. Restoring
  a control-plane backup next to copied frames no longer needs the original root to
  still exist.
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
- **The open that switches a control-plane database to MVCC now syncs the header it
  wrote.** The switch writes the MVCC header into the database file without syncing
  it, and commits then sync only the `-log`, so a power cut could leave a header that
  still says WAL beside a log of acknowledged commits, which Turso refuses to open.
  Turso synced it anyway for a database that never ran MVCC, but not for one switched
  back to WAL. The open now commits a no-op transaction and runs a TRUNCATE
  checkpoint, which makes Turso fsync the file, header included; an open of a database
  already in MVCC does neither.
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
- **`Database::new` could leave a half-created database** that neither a retry of
  `new` ("already exists") nor `Database::existing` (no `database` row) could use. A
  database is now built in a hidden `.{name}.creating-{nonce}` folder beside its final
  place and renamed to `{name}` only once its schema and `database` row are committed,
  then the data directory is fsynced. An error or crash leaves either nothing under
  `{name}` (retry `new`) or the complete database (open it with `existing`). Build
  folders left by a crash are removed by the next `new` or listing; the data directory
  gains a `.weft-creating.lock` file that keeps a sweep away from a creation in
  progress, in this or another process. `new` now refuses a name of the build folders'
  form (`.{x}.creating-` followed by 32 hex digits). The rename is the commit point:
  if the data directory cannot be fsynced after it, or the published database cannot
  be opened at its final path, the database stays in place (another task or process may
  already have opened it) and the error says it was created and to open it with
  `existing`; after a failed fsync it also says the creation may not survive a power
  loss. The checkpoint after creation is best-effort.
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
  failed the ingest or the consumer's dequeue. Every ingest path's enqueue
  (`capture_measurement`, `batch_capture_measurements`, `capture_new_measurement`,
  `capture_measurement_chunk`, `capture_new_measurement_chunk`) and the consumer's
  dequeue now retry such a conflict, for about three seconds; a single
  `enqueue_unbatched_measurements` call returns it as `Error::TransientMvccError`. The
  consumer dequeues in transactions of at most 5,000 entries, so an enqueue waits for
  one of them, not the whole dequeue. `batch_capture_measurements` queues all of its
  timestamps in one transaction before it stores any row, so a consumer dequeuing some
  of the same timestamps (a re-import of timestamps still queued) waits for all of it,
  and either side can run out of retries: the build fails, or the import fails before
  storing anything. Both are safe to re-run. The paths without a write-ahead enqueue
  (`capture_new_measurement` and the two chunk methods) still queue after their commit,
  so an error there fails the call although the rows are stored.
- **The incremental build interpolated windows that could not be complete.** A queued
  timestamp past the latest stored row (an append in progress, or the write-ahead
  entries of one that failed or was abandoned, up to a whole call's worth) stays queued
  until rows reach its windows, and every build interpolated each of those windows only
  to find it short. The build now reads the latest stored measurement once per run and
  skips a window that ends after it; the timestamps stay queued as before.
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
  table of the processed batch database (a hash and a time per batch, indexed by hash),
  in the transaction that deletes them. That table grows by about one row
  per resolution step of the aspect, since extraction consumes about one sliding-window
  batch per step, so it is bounded: each extraction also deletes the rows older than
  `WEFT_EXTRACTED_BATCH_RETENTION_SECS` (48 hours by default: about 2,880 rows for a
  minute-resolution aspect, 172,800 for a second-resolution one), and
  `clear_processed_batches` (which `Pipeline::prepare_data_full_rebuild` calls) deletes
  them all, so a full rebuild still extracts every window again. The rebuilds the record
  guards against come on the first build after an ingest that overlapped a pipeline
  run, or on the re-run after a build crashed; one that comes after the record expired
  (a pipeline run more than the retention after the previous one, or a longer crash
  recovery) extracts those windows a second time. A first-run full rebuild
  (`is_first_run`) now skips the windows extracted within the retention; older ones are
  batched and extracted again, as before. The unprocessed and processed batch databases
  gain an index, `idx_batches_hash` on `batches(aspect_id, batch_hash)`, and
  `extracted_batches` one on `batch_hash`, created when a process first opens them, so
  each check is a point lookup.
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
