# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the project is pre-1.0, minor version bumps may contain breaking changes.

## [Unreleased]

⚠️ **Upgrading a segment store from v0.1.0 is one-way.** The first open by this
version migrates a v0.1.0 store in place to store layout v2: it writes a
`STORE_FORMAT` marker, adds columns and tables to `segment_index.db`, and from then on
segment ids come from a persistent allocator and maintenance writes new, journaled
frames instead of rewriting them in place (see Added and Changed). v0.1.0 reads no
marker, so it would not refuse a migrated store, but it knows none of this: once this
version has opened a store, do not run v0.1.0 on it again. Before upgrading, stop
`weft-server` and copy the whole store directory; `POST /api/v1/storage/backup` copies
only the control plane, not the segment frames.

### Added

- **Write poison** (crash-consistency design, S6). When a segment-index COMMIT fails
  in a way that may still have committed (any COMMIT error except a conflict Turso
  detects while validating the transaction, which it rolls back before writing
  anything), the transaction may or may not be durable, so the store now refuses every
  write (seal, declare, reconcile, split, merge, squash, compact, rollup rebuild) with
  the new `weftdb::Poisoned` error until the process restarts, while reads keep
  working; the restart's open settles the transaction. `SegmentStore::poisoned()`
  reports it, and `GET /ready` gains `poisoned`, `restart_required` and
  `poison_reason`. `WEFT_ON_AMBIGUOUS_COMMIT=exit` makes `weft-server` log and exit
  with status 70 once its store is poisoned, for deployments whose supervisor restarts
  it (unset or `poison` keeps the default); the library itself never exits the process
  (see the options entry under Changed). `SegmentStore::wait_until_poisoned()` and
  `subscribe_poison()` let an embedder do the same.
- **Store format marker and migration registry** (freeze design §4.3, FRE-12a). A
  segment store root now carries a `STORE_FORMAT` file (plain JSON: `layout_version`,
  `min_read_layout`, `min_write_layout`, `last_written_layout`, `migrating_to`,
  `applied_through`, `store_uuid`, `scope`), written through the durable I/O layer
  (temporary file, fsync, rename, directory fsync). The open reads it right after it
  takes `LOCK` and before it opens any database, and refuses a store whose
  `min_write_layout` is newer than this build's layout (`weftdb::SUPPORTED_LAYOUT`,
  now 2) with `weftdb::StoreError::IncompatibleLayout`, having touched nothing but
  `LOCK` and `LOCK.holder`; an unparseable marker is `StoreError::UnreadableStoreFormat`.
  A new store writes its marker before any database file exists. Every control-plane
  table is now created by a registered, idempotent migration (`0001_baseline`: the
  pre-1.0 schema; `0002_s6_s7`: layout 2), recorded in a new `store_migrations` table
  of `segment_index.db`; the open runs every pending migration's precheck first, raises
  the marker's floors before it applies one, so a crash half way never leaves floors an
  older WeftDB would not respect, and mirrors the marker into `store_meta`. A root
  without a marker but with databases is judged by `store_meta` before anything is
  written to it: a newer write floor recorded there is `IncompatibleLayout`, a lost
  marker is rebuilt from that copy (keeping the store's layout, floors and identity),
  and a store written before markers existed (any v0.1.0 store) is layout 1 and is
  migrated in place, one way (see the upgrade note at the top of this section). A
  store whose layout is newer than this build's but whose write floor it meets opens
  without migrating (recovery is told to only report); a root holding only such a
  marker, with no database, is refused with `StoreError::NewerStoreWithoutDatabases`
  and nothing created. No open function runs DDL any more, and the control-plane
  databases are opened only by a store (see Changed).
- `SegmentStore::open_report()` (`weftdb::OpenReport`: `created_new`,
  `migrated_from`, the migrations `applied`, `recovery_report_only`),
  `SegmentStore::store_uuid()` and `SegmentStore::store_format()`. `weft-server`
  prints whether it created, opened or migrated its store, with the store's UUID and
  layout.
- `segment_index.series_id` (`INTEGER NOT NULL DEFAULT 0`, 0 being the empty tag set)
  and `aspect_seq.next_series_id` (`DEFAULT 1`), in migration `0002`, so tagged
  series need no table rebuild later (tags A7). Every row reads back as series 0.
- `weftdb::exec::control_plane_write` and `weftdb::durable::poison_global()`
  (robustness track ROB-2): every segment-index transaction runs inside the
  task-local control-plane write scope, so a panic hook can tell a panic in the middle
  of a control-plane write from one in a read, and the process-wide poison it would set
  refuses every write of every store, as a store's own poison does, while reads go on.
  It refuses every store open too (`weftdb::Poisoned`), since an open writes.
- `LOCK.holder` records the holder's host name and boot id beside its pid, and a
  second opener's error names the host (`… in use by pid 1 on host db-2 (session …)`).
  The short wait for a lock that a child being spawned still shares now applies only to
  this process on this host in this boot: another container on the same volume, also
  pid 1, is refused at once.
- On Apple targets every control-plane connection sets `PRAGMA fullfsync=ON`, and the
  open checks it beside `synchronous=FULL`: Turso syncs a COMMIT with plain `fsync`
  there otherwise, which does not survive power loss.
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
- `weft-server` has a `fault-injection` feature too (it enables weftdb's), for the test
  that `GET /ready` reports a poisoned store:
  `cargo test -p weft-server --features fault-injection --test ready_poisoned`. Under
  that feature `SegmentStore` has a hidden `inject_poison` test hook.
- `MaintenanceBusy`, `MaintenanceWait`, `DEFAULT_MAINTENANCE_WAIT`,
  `SegmentStore::maintenance_wait` and `SegmentStore::with_maintenance_wait` (see the
  maintenance entry under Changed), and `SegmentStore::index_conflicts()`, the number of
  segment-index transaction attempts that lost an MVCC conflict since the store opened,
  which stays at zero unless something outside the store contends for the index. Under
  the `fault-injection` feature `SegmentStore` has a hidden `hold_maintenance` test hook,
  used by `cargo test -p weft-server --features fault-injection --test maintenance_busy`.
- `SegmentStore::reap(MaintenanceWait)` (`weftdb::ReapSweep`): runs the reaper of the
  frame journal (see the write-once maintenance entries under Changed) over every aspect
  with journal rows, unlinking each frame a maintenance operation retired once no
  running read and no backup may still need it, and each output an operation journaled
  but never swapped in. Every maintenance operation reaps after itself, and every open
  replays the whole journal, so this is for frames a long read still held, that the
  filesystem asked to retry later, or that an operation stopped part way (its future
  dropped, a request cancelled say) left behind; the next maintenance operation on such
  an aspect also settles those first.
- `weftdb::aspect_name::encode` and `decode`: the file-name form of an aspect name in the
  write-once frame names (crash-consistency design §4): `[a-z0-9_]` as they are, every
  other byte as `%XX` in uppercase hex, injective even where the filesystem folds case
  or normalises Unicode.
- **Series tags: `TagSet`, `SeriesKey` and `SeriesSelector`** (B-tags, TAG-1), in the
  new `weft_physical_type::tags` module and re-exported by `weftdb` (also as
  `weftdb::tags`). Nothing stores or reads tags yet; this is the one validated form
  that frames, the control plane, ingest and reads will share. The canonical form and
  its caps are **frozen format**:
  - a key is 1–128 bytes, starts with an ASCII letter or `_` and continues with ASCII
    letters, digits, `_`, `.` or `-`; the prefix `__` is reserved for system
    dimensions;
  - a value is 1–256 bytes of UTF-8 with no C0 control character (U+0000–U+001F) and
    no U+007F, compared byte for byte (no Unicode normalization);
  - a tag set holds at most 16 tags, with no key twice. An empty value or a repeated
    key is an error, never dropped or merged;
  - the canonical key (`SeriesKey`) is the tags sorted bytewise by key, each written
    as key, 0x1F, value, joined by 0x1E, and is at most 1,024 bytes. The empty set is
    the empty string and names series 0. Neither separator can occur in a key or a
    value, so the form is injective without escaping, and a frame's binding to its
    series will be a byte compare.

  `TagSet::from_pairs` takes pairs in any order and rejects every rule break with a
  typed `TagError` that names the key and the limit; the enum and each of its variants
  with fields are `#[non_exhaustive]`.
  `SeriesKey::from_canonical` reads stored bytes back and rejects bytes that are
  unsorted, repeat a key, break a rule (non-UTF-8 values included) or exceed the cap.
  Unlike input, it accepts a reserved `__` key, so that a frame a later version writes
  with a system dimension stays readable. `SeriesSelector` holds equality matchers
  joined by AND, at most one per key, where `None` means "tag absent".
  `SeriesSelector::new` selects every tag set the matchers hold for;
  `SeriesSelector::exact` also requires that the tag set has no other tag. With no
  matchers they are `SeriesSelector::all` (every series) and `SeriesSelector::untagged`
  (series 0 only). The caps and separators
  are public constants (`MAX_TAG_KEY_BYTES`, `MAX_TAG_VALUE_BYTES`, `MAX_TAGS`,
  `MAX_SERIES_KEY_BYTES`, `TAG_KEY_VALUE_SEPARATOR`, `TAG_PAIR_SEPARATOR`,
  `RESERVED_TAG_KEY_PREFIX`).
- **Integer-native exact-decimal reductions** (`weft-reduce`). `reduce_scaled` and
  `reduce_partial_scaled` reduce `ScaledI64` mantissas at one scale, with epoch-nanosecond
  instants, in integer accumulators and build one `BigDecimal` per bucket; their buckets
  equal `reduce` / `reduce_partial` on the same data. `reduce_scaled` covers `min`, `max`,
  `avg`, `sum`, `first` and `last`; `reduce_partial_scaled` also takes the `sketch_p*`
  reductions, with identical sketches. Both return `Ok(None)` for a reduction they do not
  cover, so the caller falls back.
- **Physical-value and batch reads** (`weft-physical-type`).
  `weftseg::read_segment_range_physical` and `read_paged_segment_range_physical` return
  each present value as its stored `PhysicalValue` (for `ScaledI64`, the mantissa and
  scale) instead of a `BigDecimal`. `weftseg::read_values_at` reads many indices of a
  value block in one walk of its block chain, through the new
  `timestamp::{blocked_bitpack_decode_gather, for_bitpack_decode_gather}` (and, with
  `bitsliced-codec`, `transpose_bitpack_decode_gather`). The blocked and bit-sliced ones
  decode a block whole once `timestamp::GATHER_WHOLE_TILE_THRESHOLD` requests land in it
  and value by value below that.
- **Advisory size estimates for three codecs not written to disk** (`weft-physical-type`):
  the decimal-exponent FOR value codec, which factors each block's shared power of ten out
  before FOR packing (`timestamp::dfor_bitpack_{bytes,encode,decode,decode_range}`,
  `ColumnEncoding::dfor_value_bytes`), and two timestamp-column estimates,
  `DeltaOfDeltaColumn::common_multiple` / `common_multiple_estimated_bytes` (the deltas'
  common factor taken out) and `delta_for_estimated_bytes` (first-order deltas under
  per-block FOR). They are not behind `experimental-codecs` and have not had the patent
  review the gated codecs are waiting on.
- **Weft-Bench runs its storage workloads on a real CSV corpus.** `--comp-csv`,
  `--pl-csv`, `--rf-csv` and `--ds-csv` feed the compression, point-lookup, range-fetch
  and downsample workloads from a CSV file instead of a generator, with
  `--csv-value-col` (0-based value column) and `--csv-skip` (data rows skipped after the
  header). Reports gain `storage.advisory_dfor_value_bytes` (bench schema v17),
  `storage.advisory_common_multiple_timestamp_bytes` and
  `storage.advisory_delta_for_timestamp_bytes` (v18), each present only when it beats the
  realized codec, and `metadata.work_disk_kind`, `work_disk_file_system` and
  `work_disk_mount_point`, the disk under the working directory (v19).
- **Weft-Bench's downsample generator emits two-decimal values** (`--ds-decimals`, default 2;
  `full` keeps the previous generator's exact binary expansion of each float). The old values
  carried ~50 significant digits and made the published reduction figures 7.3× slower than real
  data; the README table is re-measured with both generators, and its claim that `sketch_p99` is
  ~2.6× faster than exact `p99` is withdrawn (level on two-decimal values, 1.36× on real BTC).
- **Gap filling** (`weft-reduce`): `weft_reduce::fill` turns a reduction's buckets into the
  dense grid between two bounds, keeping measured buckets unchanged and synthesizing every
  empty step with `count == 0` by a declared `Fill` (`Null`, `Previous`, `Linear` by grid
  step, a constant `Value`, or `Spline`, a splimes spline through the bucket values;
  `Fill::from_token` parses `null`/`prev`/`linear`/`quadratic`/`cubic`/a decimal),
  bounded by a caller-given bucket limit (`FillError`).
- **Weft-Bench `--gap-fill` workload**: TSM-Bench Q5's `SAMPLE BY … FILL(LINEAR)` shape over
  a seeded series with outages (`--gf-points`, `--gf-stride`, `--gf-bucket`, `--gf-outage`,
  `--gf-outage-len`, `--gf-fill`, `--gf-agg`), gated on a dense grid and scoring the filled
  buckets against the clean signal; `--gf-csv` cuts the outages from a real series and
  scores against the real values removed.
- **Weft-Bench records the GPU driver.** An interpolation run's `metadata.engine` gains
  `gpu_driver`, the driver's name and version as splimes' `GpuInfo::driver` reports it
  (e.g. `NVIDIA 610.57.04`), beside the adapter in `gpu`; it is printed and shown in the
  HTML report, and omitted when the driver reports nothing (bench schema v20).

### Changed

- **Exact percentiles (`p50`/`p90`/`p95`/`p99`) no longer clone and sort every bucket**
  (`weft-reduce`). A single percentile selects its rank in linear time over borrowed values
  and several share one sort; only the result is cloned. Results are identical, including
  which of two numerically equal values (`1.0`, `1.00`) is returned. A lone hourly `p99`
  runs ~1.6–3.4× faster on the bench (two-decimal, real BTC and full-expansion series),
  which made it about twice as fast as `sketch_p99` until the next entry.
- **`sketch_p*` reductions convert most values to `f64` by one division** (`weft-reduce`).
  A value `m × 10^-s` with `|m| < 2^53` and `0 <= s <= 22` is converted as
  `m as f64 / 10^s`, which is bit-identical to `bigdecimal`'s `to_f64`, so sketches and
  results do not change; `ScaledI64` segments feed their mantissas straight in. Hourly
  `sketch_p99` runs ~2× faster on two-decimal and real BTC series, and a stored segment's
  `avg,sketch_p99` downsample ~2× faster on the integer path. Values outside those bounds
  take the previous conversion.
- **Breaking for Rust users: `weftdb` reads no `WEFT_*` variable when it opens a
  segment store, and never exits the process** (release plan C-1).
  `SegmentStore::open` and `open_scoped` now use `SegmentStoreOptions::default()`
  (no checkpoint index, no partial sidecars, no bit-sliced codec) whatever the
  environment says; pass `SegmentStoreOptions::from_env()` to the new
  `SegmentStore::open_with_options` / `open_scoped_with_options` to keep reading
  `WEFT_SEGMENT_*`, as `weft-server` does. `WEFT_ON_AMBIGUOUS_COMMIT` is read by
  `weft-server` alone; an ambiguous COMMIT only poisons the store, and the server exits
  with status 70 on that under `exit`. Deployments of `weft-server` see no difference.
- **Stored frame paths are resolved by splitting on both `/` and `\`**, so a store
  written on Windows reads on Unix and the other way round, and a path that would lead
  out of `segments/` (a `..`, no `segments` component) is refused instead of read
  where it points. Upgrading a layout-1 store whose index records such a path fails
  with `StoreError::UnsafeLegacyPath`, naming the aspects, before any migration applies
  or a marker is written; nothing is moved or quarantined.
- **Breaking for Rust users: `SegmentIndexStore`, `AspectMetadataStore`, `AspectCatalog`
  and `CatalogStore` can no longer be opened on their own.** Their `open` and
  `open_in_memory` are gone: they created this build's schema in any file, without the
  store's `STORE_FORMAT` gate, which would let a WeftDB write tables into a store a newer
  one wrote. A `SegmentStore` opens all four (`index()`, `metadata()`, `catalog()`,
  `registry()`); nothing in the workspace opened them otherwise.
- A store root's parent that cannot be fsynced because its filesystem refuses (`EROFS`,
  `EINVAL`, `ENOTSUP`: read-only mounts, FUSE and Docker Desktop bind mounts) is now
  skipped with a warning when the root predates the open, as an unreadable parent
  already was.
- Adding a missing column during a migration now ignores only Turso's exact
  duplicate-column error for that column; any other failure fails the open (the
  `metadata.db` order-health column used to swallow every error).
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
- **A backup label that is already taken is a `409` with `"code": "already_exists"`**
  (it was a `400`); the existing backup is untouched. `weft_server::StorageError` gains
  the `AlreadyExists` variant for it, and the JSON error body carries a `code` field
  for that error only.
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
  records `layout_version = 2` and a `store_uuid`. ⚠️ The migration is one-way:
  v0.1.0 would still open a migrated store, ignoring the additions, but do not run it
  on one again (see the upgrade note at the top of this section). A WeftDB refuses a
  store whose `layout_version` is newer than it knows, before it writes anything to its
  databases (the open has already taken the root's `LOCK`, and created `segments/` if
  it was missing).
- Segment-index writes (seals, reconciles, splits, merges, squashes) commit through
  one transaction type. A write that loses an MVCC conflict to another writer of the
  same row still fails the call at once, as before: retrying it would replay it over
  the other writer's committed row. Only a database that is busy before the
  transaction starts is retried, up to five times with backoff.
- **Segment ids come from a persistent per-aspect allocator** (crash-consistency design,
  S7). A seal took `MAX(id) + 1` for its aspect; it now takes the next id of an
  allocator that hands each id out once, writes its frame, and commits its row with a
  plain `INSERT` (never `INSERT OR REPLACE`) together with the allocator's raise, which
  is persisted in `segment_index.db`'s `aspect_seq` table. A deleted segment's id, even
  the highest, is never reused, before or after a restart, and neither is the id of a
  frame a seal wrote but crashed before committing: the allocator starts above every
  `{aspect}-{id}.weftseg` (and `.weftpart`) name in `segments/`. Ids still grow, but no
  longer as `MAX(id) + 1`: after a squash or merge deleted the highest segments, the
  next seal's id leaves a gap. The suffixes that splits and split overlap merges create
  take their ids from the same allocator. Each aspect's control-plane writes (a seal's
  row and rollup fold, maintenance row rewrites and deletes, rollup rebuilds) take turns
  under a per-aspect lock, so they no longer conflict with one another, and a seal's
  index transaction is retried (up to five times) if anything else does conflict with it.
- **Maintenance operations on one aspect take turns.** A reconcile, split, overlap
  merge, squash or compaction now holds its aspect for its whole run. The HTTP
  maintenance endpoints (`POST /api/v1/storage/{aspect}/reconcile`, `…/squash`,
  `…/compact` and the store-wide `POST /api/v1/storage/reconcile`) wait up to 30 s for
  an aspect another operation holds and then answer **`409 Conflict`**, naming it; the
  store-wide sweep maintains the other aspects first. The background reconcile daemon
  never waits: it skips a busy aspect until its next tick (logged at `DEBUG`, counted
  in the tick span's `busy` field, not as a failed pass). ⚠️ Library API: the store-wide
  sweeps (`reconcile_all_over_threshold`, `reconcile_all_hot_cold`,
  `reconcile_all_overlaps`, `reconcile_all_overlaps_with_policy`,
  `squash_all_over_threshold`, `squash_all_to_target_rows`,
  `squash_all_to_target_rows_if_fragmented`) take a new last argument,
  `MaintenanceWait::Skip` or `MaintenanceWait::Wait(duration)`, and their results gain
  a `busy` list of the aspects they could not take. The per-aspect entry points keep
  their signatures, wait up to `SegmentStore::maintenance_wait()` (30 s,
  `DEFAULT_MAINTENANCE_WAIT`; change it with `with_maintenance_wait`), and then fail
  with the new `MaintenanceBusy` error.
- **Reconcile and split are write-once** (crash-consistency design §5.3-5.4, S8).
  `reconcile_segment` (and every reconcile pass built on it) and `split_segment` no
  longer rewrite a `.weftseg` frame. Their outputs are new frames in `segments/` named
  `{enc(aspect)}~g{gen}~p{prec}.weftseg` (the aspect name encoded as by
  `aspect_name::encode`, a per-aspect generation that is never handed out twice, and the
  adoption order; an aspect name that would encode past the file-name limit is cut and
  completed with a hash, `…~h{16 hex digits}~g…`), journaled as pending in
  `segment_index.db`'s `frame_journal`, written and fsynced, then `segments/` fsynced.
  One `segment_index.db` transaction then swaps them in: it replaces the segment's row
  only if it is still the version that was read (id, generation and frame CRC; another
  version fails the operation with a `Conflict` and writes nothing back), inserts a
  split's suffix under an id allocated inside it, journals the old frame as retired,
  and advances the aspect's epoch and generation counter. The old frame is unlinked by
  the reaper once no read that may still open it is running (every read pins the
  reclaim epoch it started in, released when its future is dropped, timed out or not),
  then its journal row is deleted; a frame the filesystem refuses to unlink for now (a
  Windows sharing violation, `EBUSY`) waits for a later pass instead of failing anything.
  Every open replays the journal before it returns: outputs a crash left before their
  swap are unlinked, retired frames no row references are unlinked, `segments/` is
  fsynced, the rows are deleted, and the rollup of an aspect whose swap committed is
  rebuilt. A reconcile also checks that the frame it reads is the one its row was
  committed with (its length, and its CRC once bound), and the sidecars of its outputs
  are written under a `.tmp-*` name and renamed into place. The overlap merge, squash
  and compaction became write-once too (see the next entry). An older layout-2 WeftDB
  reads the new frames (the index keeps recording root-joined paths) but does not
  replay the journal.
- **The overlap merge, squash and compaction are write-once** (crash-consistency design
  §5.3 and §7, S9). `reconcile_overlaps`/`reconcile_overlaps_with_policy` (both the full
  rewrite of a component and the split of its cold prefix), `squash_aspect` and
  `squash_aspect_to_target_rows`, and every sweep and threshold variant built on them, no
  longer rewrite a segment's frame in place and then delete the other members in commits
  of their own. Each component, squash or compaction group is now one swap, as a
  reconcile's is: its output frames are written under new names, fsynced, and
  `segments/` fsynced, then one `segment_index.db` transaction replaces the members'
  rows with the outputs' and deletes the members left over, each only if it is still the
  version that was read (a member another writer changed fails the swap with a
  `Conflict`, writing nothing), and journals every member's frame as retired for the
  reaper. The outputs' rows record the frame actually written, so a paged member merged
  into a single-block output never leaves a row describing the wrong frame kind, and
  carry the largest adoption order (`prec`) of their members. Merges also check that each
  member's frame is the one its row was committed with, and merge on a blocking thread.
  Outputs take their members' ids, the `j`-th output in time order the `j`-th smallest
  (release plan D17, without a change to the `segment_index` key), and are cut only at
  timestamp boundaries, so a run of equal timestamps never spans two outputs.
- ⚠️ **An overlap merge that splits its component no longer allocates an id for the
  suffix.** The hot suffix takes the component's second-smallest segment id (the cold
  prefix keeps the smallest), where it used to take a fresh id from the allocator. The
  segment ids an overlap split leaves, and those later seals receive, change
  accordingly; the merged data does not.
- ⚠️ **`split_segment` refuses a split whose suffix a newer segment overlaps.** The
  suffix takes an id above every segment of the aspect, so where it shares a timestamp
  with a segment sealed after the split one it would outrank that segment's newer
  value. The split now fails with an error, changing nothing, when any segment with a
  higher id overlaps the suffix's span (its first timestamp at or after `boundary` to
  the segment's last); the check runs again under the aspect's commit lock right before
  the swap, against a seal that committed meanwhile.
- **The stored-range downsample reduces `ScaledI64` segments on their mantissas.**
  `SegmentStore::downsample_range` (behind `/api/v1/storage/{aspect}/downsample` and its
  CSV, Arrow and Parquet forms) reads a segment's window in its physical encoding and uses
  `reduce_partial_scaled` when every present value in it is a `ScaledI64` at one scale and
  every requested reduction is a streaming one or a `sketch_p*`; anything else takes the
  `BigDecimal` path as before. Results are unchanged.
- **Every reduction computes `avg` by integer long division**, in `weft-reduce`'s `reduce`
  and partials as well, whenever the bucket sum's unscaled integer fits an `i128`. The
  quotient equals `bigdecimal`'s own division in value and representation (scale
  included); a larger sum still divides as a `BigDecimal`.
- **Faster reads, same bytes on disk** (`weft-physical-type`). The frame checksum is
  computed sixteen bytes per step (slicing-by-16, the same CRC-32, so every stored frame
  still verifies); the bit-packed value and timestamp codecs, and the f64 codecs behind
  `experimental-codecs`, read a whole field per load instead of a bit at a time; the
  batch point read (`read_segment_points`, `/api/v1/storage/{aspect}/at-multi`) walks each
  value block once instead of once per instant; and the bit-sliced tile decoder
  (`bitsliced-codec`) is branch-free. The bench schema is now v19.

### Deprecated

- `SegmentIndexStore::next_id` (`MAX(id) + 1`): it reissues the id of a deleted or
  crashed segment and races concurrent callers. `SegmentStore` no longer uses it; it is
  kept for tests.

### Fixed

- **Power loss during or after a reconcile or split could lose or tear rows.** Both
  rewrote the segment's frame in place with an unsynced write (a split also wrote its
  suffix that way), so a power cut, even after the call returned, could leave a
  committed row over an empty, torn or zero-filled frame: its rows gone and every read
  over them failing with a checksum error. Their outputs are now fsynced, under new
  names, before the commit that switches to them (see Changed).
- **A read during a reconcile or split could fail or see rows twice.** A read could open
  a frame half way through its in-place rewrite, and a split committed its suffix before
  it shrank its prefix, so a read in between returned the suffix's rows twice. A swap is
  now one transaction, and the frames it retires stay until no read that may open them
  is running.
- **Power loss during or after an overlap merge, squash or compaction could lose or tear
  rows.** Each rewrote its lowest segment's frame in place with an unsynced write, then
  deleted the other members' rows and frames one by one, so a power cut, even after the
  call returned, could leave the merged row over an empty, torn or zero-filled frame,
  with the members whose rows it had absorbed already deleted: their rows gone and reads
  over the merged range failing with a checksum error. A crash between the rewrite and
  the row update could also leave a paged segment's row over a single-block frame. Each
  merge is now one swap of fsynced new frames (see Changed).
- **A read during an overlap merge, squash or compaction could fail or see rows twice.**
  Between a merge's rewrite of its lowest segment and the deletion of the others, a read
  returned the merged rows beside the members' own, and a read during the rewrite could
  open a half-written frame. The swap is now one transaction, and the frames it retires
  stay until no read that may open them is running.
- **An overlap merge's split suffix could outrank a newer seal.** The suffix took a fresh
  id from the allocator when the merge wrote it, above that of a seal that had taken its
  id meanwhile, or before the merge but committed after it, so where the two shared a
  timestamp the merge's older value won over the seal's acknowledged one. The suffix now
  takes a member's id, below the seal's.
- **A reconcile whose segment another operation changed meanwhile wrote it back.** Its
  swap now requires the segment's row to be the version it read and fails with a
  `Conflict` otherwise (the maintenance lock already keeps the store's own operations
  apart; this holds for anything else that writes the index).
- **Concurrent seals of one aspect could lose an acknowledged batch.** Two seals that
  ran at once took the same id: the later one's write replaced the earlier one's frame
  and its row, so a batch whose seal had succeeded was gone, or one of the seals failed
  with an MVCC write-write conflict. Every seal now gets an id of its own (see the
  allocator entry under Changed).
- **A seal could reuse the id of a deleted segment or overwrite a crashed seal's
  frame.** After a squash or merge deleted an aspect's highest segment, the next seal
  took its id again, so a seal racing the merge's unlink could lose its frame, and a
  leftover sidecar of the deleted segment could be served for it. A seal that crashed
  after writing its frame and before committing it left the frame behind, and the next
  seal truncated it.
- **A reconcile racing a squash or merge of the same aspect could bring a merged
  segment back.** A reconcile that had read a segment wrote it back after the merge
  had folded it into its target and deleted it, so its stale values outranked the
  merged ones and its rows read twice. Maintenance operations on one aspect now take
  turns (see Changed).
- **Concurrent seals could leave the aspect's rollup (`metadata.db`) wrong**, missing
  some of them, or fail after their row had committed: each folded itself in with an
  unlocked read-then-write. The fold, and rollup rebuilds, now run under the aspect's
  lock with the seal's commit.
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
- **Opening a control-plane database now syncs the MVCC header the switch to MVCC
  wrote.** The switch writes the MVCC header into the database file without syncing
  it, and commits then sync only the `-log`, so a power cut could leave a header that
  still says WAL beside a log of acknowledged commits, which Turso refuses to open.
  Turso synced it anyway for a database that never ran MVCC, but not for one switched
  back to WAL. Every open of each of the four databases now commits a no-op
  transaction and runs a TRUNCATE checkpoint, which makes Turso fsync the file, header
  included, before the open returns. Every open, not only the one that switched: an
  open that crashed between its switch and that sync leaves a header the next open
  reads back as MVCC although it never reached the disk. This costs one commit and one
  checkpoint per database per open.
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
- **Two restore drills started in the same millisecond shared a rehearsal directory**,
  so whichever ended first removed the directory the other was still using. A drill
  now claims its `.restore-drill-<millis>` directory before restoring into it, and one
  that finds it taken answers `409` with `"code": "already_exists"`, leaving it alone;
  retry it.
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

### Security

- **Backup and drill labels starting with `.partial-`, `.deleting-` or
  `.restore-drill-` are rejected with `400`**: those names belong to unfinished
  backups, prunes and drills, which are never restored and are swept.
- **The backup label grammar is fixed for 1.0: `[A-Za-z0-9][A-Za-z0-9._-]{0,99}`, not
  ending with `.`.** ⚠️ `POST /api/v1/storage/backup?label=` and
  `POST /api/v1/storage/restore/drill?label=` answer `400` for a label that starts with
  `-` or `_` or is longer than 100 characters; both were accepted before. The drill still
  accepts a daemon snapshot's `backup-<digits>`. An existing backup whose name falls
  outside the grammar can be drilled once renamed into it (not into `backup-<digits>`,
  which retention prunes).

## [0.1.0] - 2026-10-08

First public release, and the baseline that later releases' upgrade, rollback and
compatibility checks run against. WeftDB is pre-beta: the API will change, and the
version number says so. This release ships as pre-built binaries on the GitHub release
only; the library crates are not published to crates.io (build them from the `v0.1.0`
tag).

The Changed, Fixed and Security entries are for anyone who built WeftDB from source
before this release. A bare "0.1" in them means splimes 0.1.

### Known limitations

- **Pre-beta.** The HTTP API, the Rust API, the configuration and the on-disk format can
  change in any 0.x release. Nothing is covered by a stability promise before 1.0.
- **No authentication, authorization or TLS.** `weft-server` serves an unauthenticated,
  plain-HTTP API; see
  [`SECURITY.md`](https://github.com/basic-automation/weftdb/blob/v0.1.0/SECURITY.md).
  Do not expose it to an untrusted network: keep the default loopback bind, or put it
  behind something that terminates TLS and authenticates callers.
- **Crash consistency is incomplete.** A `2xx` from a storage ingest survives a crash of
  the `weft-server` process, but not necessarily a power loss or an operating-system
  crash: the index commit is fsynced, the segment frame it points at is not.
  Maintenance (reconcile, split, squash, compaction) still rewrites committed frames in
  place, so a crash during a rewrite can tear one. And two seals into one aspect at the
  same moment can be given the same segment id, so that one replaces the other.
  [`docs/design/crash-consistency.md`](https://github.com/basic-automation/weftdb/blob/v0.1.0/docs/design/crash-consistency.md)
  lists the 43 windows it verified. This release closes one of them, the deleted MVCC
  log (see Fixed). The durability work after it closes or mitigates the rest, though a
  legacy (rows-mode) database keeps some of them by design.
- **The next release changes the store layout.** It moves a store to layout v2, in
  place, and the upgrade is one-way: once a newer release has written to a store, do not
  run v0.1.0 on it again. Before upgrading, stop `weft-server` and copy the whole store
  directory; `POST /api/v1/storage/backup` copies only the control plane, not the
  segment frames.
- **The Linux binaries need glibc 2.34 or newer, and `weft-tui` needs 2.39 or newer.**
  They are built on Ubuntu 24.04. Ubuntu 22.04, Debian 12 and RHEL 9 run `weft-server`
  and `weft-bench` but not `weft-tui`; on an older glibc, build from source.
- **The macOS and Windows binaries are not signed.** Gatekeeper and SmartScreen warn
  before running them. Check each archive against `SHA256SUMS` instead.
- **The bit-sliced codec is off in the release binaries.** None of them is built with
  `bitsliced-codec`, so they never write the bit-sliced value codec
  (`WEFT_SEGMENT_TRANSPOSED_MAX_OVERHEAD` is ignored, with a warning) and cannot read a
  segment written with it; for a store that has such segments, build `weft-server` from
  source with `--features bitsliced-codec`. `experimental-codecs` is compiled into
  `weft-bench` only, for its advisory size estimates; nothing writes those codecs to
  disk.

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
  bit-packing and delta-of-delta timestamp coding (an opt-in bit-sliced value codec sits
  behind the `bitsliced-codec` feature; see Changed), over a Turso (libSQL) control
  plane for catalog, metadata and the segment index.
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
- Pre-built `weft-server`, `weft-tui` and `weft-bench` archives for Linux (x86_64 and
  aarch64), macOS (x86_64 and arm64) and Windows (x86_64), with a `SHA256SUMS` file,
  attached to the GitHub release.
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
- **Contribution terms.** Every pull request takes one of two routes, the contributor's
  choice: a DCO sign-off (`git commit -s`) on every commit, or the WeftDB Individual
  Contributor License Agreement
  ([`CLA.md`](https://github.com/basic-automation/weftdb/blob/v0.1.0/CLA.md), version 1,
  adapted from the Apache Software Foundation's ICLA with Justin Icenhour as the
  recipient), signed once by a pull request comment. The `contribution-terms` check
  passes a pull request when either holds. Contributions are licensed
  `MIT OR Apache-2.0`. See
  [`CONTRIBUTING.md`](https://github.com/basic-automation/weftdb/blob/v0.1.0/CONTRIBUTING.md#contribution-terms).
- **`Inputs::register_dictionary_if_absent`** registers a dictionary as
  `set_dictionary_metadata` does unless a complete registration of that name exists,
  checking inside its own write transaction, and returns whether it registered it. A
  registration that cannot be read counts as existing and is left alone, and a write
  that loses an MVCC conflict is tried once more after a short random delay. The check
  sees registrations committed before its transaction began: one committed while it
  runs, like a second writer registering the same new dictionary at the same moment, is
  not seen, both are written, and reads pick the newest.

### Changed

- **Crates renamed for publication.** The core crate is now `weftdb` (was `database`,
  a name already taken on crates.io) and the pipeline crate is `weft-orchestration`
  (was `database_orchestration`). Import paths change accordingly:
  `use database::…` becomes `use weftdb::…`, and `use database_orchestration::…`
  becomes `use weft_orchestration::…`.
- **The workspace builds on stable Rust** (MSRV 1.95). The
  `#![feature(stmt_expr_attributes)]` gate is gone, so nightly is no longer required to
  build, test or depend on any crate. Only `cargo fmt` still uses nightly, because
  `rustfmt.toml` sets nightly-only options.
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
  with `AspectStructure::new_dictionary`, or registered by a pipeline through
  `weft_orchestration::load_dictionary` (which stored no constraints before this
  release; see Fixed), persists its step interpolation as the `Spline`'s text
  (`steps_interpolation` in the `dictionary_constraints` table of
  `<aspect>/dictionaries/<dictionary>.db`). splimes 1.0's `Spline::from_str` validates what it parses, so a stored
  `Polynomial` with degree 0, a degree above 8, or a negative or non-finite bounds
  factor, which 0.1 accepted, no longer loads: `get_dictionary_metadata` returns
  `Database error: Invalid interpolation format: invalid polynomial degree 9: must be
  between 1 and 8` (or `… invalid bounds factor -1: must be finite and not negative`),
  and `load_dictionary` logs that error as a warning ("Failed to read dictionary
  metadata; leaving the stored registration as it is") and carries on with the
  pipeline's own constraints, without touching what is stored. Until this release `get_dictionary_metadata` could
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
  be between 1 and 8`; it stored any method before. `set_dictionary_metadata`, and so
  `load_dictionary` when it registers a dictionary, refuses one the same way.
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
- **Store-wide maintenance carries on past a failing aspect.** Every store-wide sweep
  (reconcile, overlap merge, squash and compaction, from the background daemon or
  `POST /api/v1/storage/reconcile`) stopped at the first aspect whose pass failed, on a
  torn or truncated frame for example, and so never maintained the aspects after it in
  name order. Each aspect's failure is now its own: the endpoint answers `200` and lists
  the aspects that failed in `failed: [{aspect, error}]` (it answered `500`), the daemon
  logs one `WARN` line per failed aspect, and both count them in
  `weft_reconcile_failed_passes_total`. Only an unreadable aspect list is still a `500`.
  For Rust callers, `ReconcileSweep`, `HotColdSweep`, `OverlapSweep` and `SquashSweep`
  gain `failed: Vec<(String, anyhow::Error)>` and are no longer `Copy`, `Clone`,
  `PartialEq` or `Eq`.
- **Turso control plane upgraded 0.6 → 0.8.** ⚠️ This is one-way: once 0.8 writes a
  store, its MVCC log is v3 and an older WeftDB can no longer open it. Back up the
  control plane before upgrading.
- **`.weftpart` sidecars use `postcard` instead of `bincode`** (frame v3). Sidecars
  written by older versions are ignored: the segment is decoded instead, and the
  sidecar is rebuilt on the next seal. No data migration is needed.
- `weft-bench --spline poly:N` rejects a degree outside 1–8 when the arguments are
  parsed, naming the limit. It accepted any degree before, and with splimes 1.0 such a
  run would only have failed once the engine rejected it.
- All dependencies updated to their latest major versions, including wgpu 30,
  Arrow/Parquet 60 and OpenTelemetry 0.33.
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
  variable is ignored with a warning. For library callers, without the feature
  `FrameOptions::transposed_max_overhead` is accepted but silently ignored
  (`write_segment_with` and `write_paged_segment_with` emit the size-selected codec), and
  `WeftSegError` gains the `CodecNotEnabled` variant, which breaks exhaustive matches on it.
  The `transpose_bitpack_*` primitives, `TRANSPOSE_TILE`,
  `ColumnEncoding::{transposed_value_bytes, transposed_overhead, best_value_codec_transposed}`
  and `weftseg::write_value_column_transposed` need the feature too. The default
  configuration never wrote this codec, so a store that never set the variable (or
  `FrameOptions::transposed_max_overhead`) is unaffected. The docs now call it a bit-sliced
  (bit-plane-major) layout; it is not the FastLanes layout they used to name.
- **WeftDB is dual-licensed under MIT OR Apache-2.0**, at your option, from this release on.
  Every crate declares `license = "MIT OR Apache-2.0"`, and the repository root and every
  crate ship `LICENSE-MIT` and `LICENSE-APACHE` in place of `LICENSE`. The release archives
  carry both files, `NOTICE` and the `THIRD-PARTY-NOTICES` files. Earlier commits remain
  available under MIT, as they were published. The portions adapted from Chimp and
  DDSketch stay under Apache-2.0 whichever option you choose.
- The MIT license text (now `LICENSE-MIT`) reads `Copyright (c) 2025-2026 Justin Icenhour`,
  naming the individual copyright holder for both years of the project.

### Fixed

- **A restart lost a legacy database's committed metadata.** The first open of a
  legacy (rows-mode) database in a process deleted its Turso MVCC log, `<file>.db-log`.
  Under MVCC that log holds every committed transaction until a checkpoint copies it
  into the main file, and WeftDB's checkpoints do not run under MVCC, so for
  `metadata.db` it was usually the only copy: every commit made before a restart (the
  database row, subjects, aspects, the unbatched queue) was lost, and
  `Database::existing` then failed with `no such table: database`. A non-empty log is
  now kept, and Turso replays it at open; only an empty one is removed.
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
- **A pipeline's dictionaries lost their steps and variabilities, and gained a metadata
  row on every load.** `set_dictionary_metadata`, which
  `weft_orchestration::load_dictionary` registers a pipeline's dictionaries with,
  inserted only the `dictionary_metadata` row. The steps and variabilities were never
  stored, `get_dictionary_metadata` (which needs the constraints row) never found the
  dictionary, and so every `load_dictionary` (two per `Pipeline::extract_patterns`)
  registered it again, with another row and a new id. It now writes the whole
  registration and replaces any earlier one of that name, since the tables cannot carry
  a unique constraint; `AspectStructure::new_dictionary` writes the same way, so creating
  a dictionary twice no longer leaves two. `load_dictionary` registers a dictionary only
  when none is registered, under the `Dictionary`'s own id, through the new
  `Inputs::register_dictionary_if_absent`: it checks again inside its write, so a
  registration committed after `load_dictionary` read none and before that write began
  (an explicit `set_dictionary_metadata` with tuned constraints, say) is kept instead of
  replaced with the pipeline's, and a write that loses an MVCC conflict to another load
  healing the same rows is tried once more. A dictionary name that cannot name a file is an
  error from `load_dictionary`. `get_dictionary_metadata`
  now answers `Ok(None)` for a dictionary with no database yet (it was an error,
  `Dictionary '…' does not exist for aspect '…'`), and a registration it cannot read
  is left alone instead of registered again. Of several rows for one name,
  `get_dictionary_metadata` reads the newest that has constraints (it read whichever
  came first), so a dictionary an earlier version registered from a pipeline reads as
  unregistered and is registered again, with its constraints, on its next load, which
  also deletes its extra rows.
- **`get_dictionary_metadata` returned no variabilities.** It always answered `None`,
  although `new_dictionary` stores them. It now reads them back in the order they were
  stored; an empty list stores nothing, and so reads back as `None`.
- **`list_dictionaries` read only a dictionary named "default".** It opened the
  aspect's dictionary named "default", so it failed for an aspect without one
  (`Dictionary 'default' does not exist for aspect '…'`) and otherwise listed only that
  dictionary. Each dictionary is its own `<aspect>/dictionaries/<name>.db`; it now lists
  all of them, by name, each with its steps and variabilities (it returned empty
  constraints), and the list is no longer cached, so it does not go stale when a
  dictionary is added. Only regular files are listed: a symlink, which it followed and
  so opened (setting the target's journal mode and creating tables in it), a directory
  or another entry named `*.db` is skipped. Every regular `*.db` file there is still
  opened as a dictionary, so keep backups out of that directory or under another
  extension. A dictionary whose stored registration does not parse, such as one with a
  stored step method splimes rejects, is logged as a warning and left out instead of
  failing the whole listing; a read that fails (I/O, a query, an MVCC conflict) still
  fails it, so a dictionary is never left out only because it could not be read.
- **A dictionary file without its tables could not be registered.** A dictionary's file
  can exist before its tables, for example when `insert_pattern_into_dictionary` opened
  it first, which creates no tables. Once the file was open in the process nothing created
  them, so `get_dictionary_metadata` failed with `no such table: dictionary_metadata`,
  `load_dictionary` only warned and never registered the dictionary, and
  `list_dictionaries` failed for the whole aspect. Such a file holds no registration, so
  it now reads as unregistered (`Ok(None)`), and registering the dictionary creates the
  tables.
- **A metadata read could cache a registration that had just been replaced.** A
  `get_dictionary_metadata` whose snapshot predated a `set_dictionary_metadata` commit
  could store the old registration after the write had invalidated it, and the old one
  was then served for up to the ten-minute cache lifetime. A read now caches what it read
  only if no write on the same `Database` invalidated the entry since it began
  (`DatabaseCache::generation`, `invalidate_generation` and `store_if_generation`). A
  registration written through `AspectStructure::new_dictionary`, another `Database`
  handle or another process still does not invalidate the cache, and can be served
  stale until the entry expires.
- **Commit failures were silently ignored.** A control-plane write that lost an MVCC
  conflict was reported as success. Commit errors now reach the caller, and the
  transaction is rolled back.
- **A write that lost an MVCC conflict reported a failed rollback instead.** Turso rolls
  a transaction back itself when one of its statements loses a write-write conflict, so
  the `ROLLBACK` after the failed statement fails (`cannot rollback - no transaction is
  active`), and control-plane writes (registering or creating a dictionary, inserting a
  pattern, removing an event, …) returned that error, `Rollback failed: …`, in place of
  the conflict. They now return the statement's error and log the failed rollback as a
  warning. `weftdb::error::is_transient_mvcc_error` also matches Turso's `Busy`,
  `BusySnapshot` and write-write conflict errors anywhere in an error's chain (it
  matched only WeftDB's own `TransientMvccError`), so it recognises the conflict as
  retryable where the returned error keeps Turso's error in its chain: from registering
  or creating a dictionary (`set_dictionary_metadata`, `register_dictionary_if_absent`,
  `AspectStructure::new_dictionary`) and from a failed commit. The other control-plane
  writes still return the statement's error as text only, which it does not recognise.
- **Quadratic GPU interpolation failed on GPUs without f64 support** (Apple Silicon,
  most integrated GPUs, Windows WARP). The f32 fallback shader did not parse, so wgpu
  panicked.

### Security

- **Aspect names can no longer point outside the segment store.** An aspect's name is
  part of the file names of its `.weftseg` frames and `.weftpart` sidecars, and it was
  not checked, so a client of `weft-server` could declare a name that made sealing,
  reconciling or compacting create, overwrite or delete those files outside the store's
  `segments/` directory. Names are now validated wherever they are declared or turned
  into a path (`weftdb::aspect_name::validate`): at most 160 bytes, no `/` or `\`, no
  control characters, no leading `.`, no trailing `.` or space, and not a Windows device
  name (`CON`, `NUL`, `COM1`, `CONIN$`, …, also with an extension or a `:` suffix).
  Bidirectional controls, line and paragraph separators and invisible formatting
  characters are refused as well, so a name cannot display as a different one in
  listings and logs (the zero-width joiner and non-joiner, which some scripts need, are
  still accepted). On
  Windows, `<`, `>`, `:`, `"`, `|`, `?` and `*` are refused too, since such a name could
  never be sealed there. `POST /api/v1/storage/aspects` answers `400` for an invalid
  name, and every frame path is also checked to be a direct child of `segments/`. An
  aspect already declared under such a name was never safe to use: sealing, reading or
  maintaining it now fails with a typed `weftdb::InvalidAspectName` error (a `400` from
  the ingest, read, reconcile, squash and compact endpoints), and the store-wide
  maintenance sweeps list it in `failed` and count it in
  `weft_reconcile_failed_passes_total` while still maintaining every other aspect.
  Listing it and reading its schema or stats, which touch no files, still work. Names
  that differ only in letter case or Unicode normalisation are still accepted and can
  share frame files on a case-insensitive or normalising filesystem; that is a collision
  inside `segments/`, not a way out of it, and the planned encoded frame names remove it.
- **Dictionary names can no longer point outside the aspect's `dictionaries/`
  directory.** Each dictionary is its own `<aspect>/dictionaries/<name>.db`, and the name
  was not checked, so `../x` reached `<aspect>/x.db` and an absolute name such as `/tmp/x`
  replaced the whole path; `get_dictionary_metadata` then created that file and its
  tables, and `new_dictionary` and `set_dictionary_metadata` wrote to it. Dictionary
  names now follow the aspect-name rules (`weftdb::dictionary_name::validate`), checked by
  the dictionary path builders, so every dictionary operation (`new_dictionary`,
  `Aspect::dictionary`, `set_dictionary_metadata`, `insert_pattern_into_dictionary`,
  `get_dictionary_metadata`, `get_dictionary_db`, `get_dictionary_patterns`) refuses
  such a name with a typed `weftdb::InvalidDictionaryName` error before any file is
  touched. `Config::aspect_dictionaries_db_path` and `Config::dictionary_path` return
  `Result<String>` (they returned the path), and `list_dictionaries` skips a `.db` file
  whose name is not a valid dictionary name, with a warning. No `weft-server` endpoint
  takes a dictionary name.

  **Breaking for existing data:** a dictionary an earlier version created under a name
  these rules now refuse (a leading `.`, a trailing `.` or space, a Windows device name
  on any OS such as `aux`, `con`, `com1` or `nul.x`, a control, bidirectional or
  invisible formatting character, or more than 160 bytes) can no longer be reached.
  Every operation on it returns `InvalidDictionaryName`, `load_dictionary` fails the
  pipeline run that uses it, and `list_dictionaries` leaves it out with a warning.
  Nothing is deleted. To recover one, stop everything that uses the database, rename
  `<aspect>/dictionaries/<name>.db`, and the `<name>.db-log` and `<name>.db-wal` files
  beside it if present, to a valid name, then set that name in the `name` column of the
  dictionary's `dictionary_metadata` rows, in the `dictionary_name` column of the
  `pipeline_dictionaries` rows in the aspect's `pipeline.db`, and in the pipeline
  configuration (`PipelineRunConfig`, `PipelineBuilder`) that names it.
- **API callers can no longer make backup retention delete the daemon's snapshots.**
  `WEFT_BACKUP_KEEP` retention keeps the newest `backup-<digits>` directories by their
  embedded timestamp, and `POST /api/v1/storage/backup` created directories in that same
  form, both for a `?label=` in it and for every unlabelled backup, so a caller could fill
  the retained set and get the daemon's genuine snapshots pruned. That form is now
  reserved for the backup daemon: a `?label=` in it is a `400`, and an unlabelled backup
  is now named `manual-<unix_millis>`. A label also may no longer start or end with `.`
  (Windows drops a trailing dot from a directory name, so such a label could still land
  on a reserved name there); the restore drill's `?label=` follows the same rule.
  Snapshots taken through the endpoint are therefore never counted or pruned by
  retention; this is a behaviour change for unlabelled backups, which used to be pruned,
  so remove them by hand when no longer needed. Retention also ignores a
  generated-looking directory whose stamp is more than 24 hours past the current clock or
  past the directory's own modification time, which no daemon snapshot ever is: it is
  not counted and not removed, and each listing logs a warning naming it. A directory
  planted through the API before this release keeps its planting time as its
  modification time, so a far-future stamp stays ignored after the clock reaches it, as
  long as the directory is not modified and its filesystem reports modification times.
  One stamped less than 24 hours past its planting looks like a snapshot from a skewed
  clock and is counted, but no longer outranks new snapshots a day after it was
  planted. Remove such directories by hand. Anyone who can write to the backup directory
  directly can still affect retention; this closes the API path.
- **Web pages can no longer drive a loopback-bound `weft-server`.** With no
  authentication, the default `127.0.0.1` bind was the only protection, but a page open
  in a browser on the same machine could still send requests that need no CORS
  preflight (enough to trigger maintenance, backups and ingest), or reach the API
  through DNS rebinding. While bound to a loopback address (including the IPv4-mapped
  `[::ffff:127.0.0.1]`), the server now answers only requests addressed to `localhost`,
  `127.0.0.0/8` or `[::1]` (`421 Misdirected Request` otherwise, `/health` and `/ready`
  included) and refuses a state-changing request whose `Origin` is not a loopback origin
  (`403`). Clients that send no `Origin`, such as `curl`, are unaffected. Pages served by
  another local web server (any loopback origin, on any port) are still trusted until
  authentication lands. Health probes must send a loopback `Host` such as `localhost` or
  `127.0.0.1`, as every HTTP/1.1 client does; an HTTP/1.0 probe that sends no `Host` now
  gets `421`, so configure it to send one or set `WEFT_ALLOW_ANY_HOST=1`. Behind a local
  reverse proxy that forwards a different `Host`, or the non-loopback `Origin` of a
  browser UI it fronts, set `WEFT_ALLOW_ANY_HOST=1`. Non-loopback binds are unchanged
  and remain unauthenticated.
- Cleared the `crossbeam-epoch` (RUSTSEC-2026-0204) and `h2` (RUSTSEC-2026-0258)
  advisories, replaced the unmaintained `bincode` (RUSTSEC-2025-0141), and removed the
  unsound `lru` 0.16 (RUSTSEC-2026-0253) by disabling turso's unused full-text search.

[Unreleased]: https://github.com/basic-automation/weftdb/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/basic-automation/weftdb/releases/tag/v0.1.0
