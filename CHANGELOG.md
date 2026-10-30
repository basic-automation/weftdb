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

- **First cross-engine report** (evidence track EVI-15), in `bench/reports/evi-15/`: WeftDB
  against DuckDB `DOUBLE` and `DECIMAL` on load, storage bytes, linear interpolation and bucketed
  downsampling, over MCAR, block missingness and the UCI household-power corpus at three scales,
  on the reference SSD. All 99 correctness gates passed. DuckDB loads about 4–27× faster and downsamples
  about 3× faster at `medium`; WeftDB's store is smaller (about half of DuckDB's on the UCI data)
  and its interpolation wins hold only at `tiny` and `small` scale. A second run is kept beside the
  report, and the README records which verdicts held across both. `scripts/xbench/fetch-uci.py`
  fetches the UCI input against its checksum.
- **`HONESTY.md`: four honesty pages** (evidence track EVI-16): when WeftDB is not the right tool
  (with the measured losses: bulk load and large exact-decimal grids against DuckDB, splines on
  prices, the GPU on stored reads), the benchmark methodology (correctness gates, seeds and
  manifests, bootstrap intervals, licence-checked fair comparison), the interpolation-accuracy
  methodology, and precision. The precision page says plainly that interpolated values are computed
  in `f64`; the exact-decimal guarantee covers stored values and reductions.
- **Cross-engine report with a generated honesty section** (evidence track EVI-15R). Every
  `weft-xbench` run now writes `report.md` and a self-contained `report.html` beside
  `xbench.json`, and `weft-xbench render REPORT.json` re-renders one. The honesty section is
  computed from the results: where WeftDB wins and loses (disjoint p50 bootstrap intervals only;
  overlap is reported as noise), correctness failures first, what did not run, known limitations,
  caveats read off the run (disk, `TMPDIR`, debug build, few reps) and how to reproduce it. The
  report schema is v2 (`invocation`, `scratch_dir`, `optimized`); v1 reports still render.
- **`weft-xbench` part II: downsampling, storage bytes, `DECIMAL`, a bulk-load arm and a nightly
  smoke run** (evidence track EVI-14). Every arm now also reduces into epoch-aligned buckets
  (count, min, max, avg), gated against `weft_reduce`, and every load reports its bytes on disk.
  `duckdb-decimal` stores the knots exactly as `DECIMAL(18, s)`, and `weftdb-stored-bulk` seals one
  segment per load, the like-for-like comparison with DuckDB's single transaction. A new
  `cross-engine smoke` workflow builds and runs every arm at `tiny` nightly, failing on any gate.
  Report schema v3.
- **`weft-xbench`: the cross-engine benchmark, WeftDB against DuckDB** (evidence track EVI-13).
  A separate workspace (it builds DuckDB from C++) whose lock is seeded from the workspace's and
  checked for drift in CI. It times **load** (WeftDB seal vs DuckDB appender + `CHECKPOINT`) and
  **linear interpolation** for four arms: WeftDB in-memory, stored, stored `f64`, and DuckDB `DOUBLE`
  via `ASOF JOIN` SQL that follows splimes' extrapolation rule. Datasets are MCAR and block
  missingness at three scales, plus UCI with `--uci`. Every arm is gated against the in-memory grid
  and scored against the ground truth; any disagreement exits 1. Run with
  `TMPDIR=<fast disk> cargo run --release --manifest-path weft-xbench/Cargo.toml`.
- **Stored WeftDB benchmark adapter in `weft-bench`** (evidence track EVI-11). `--stored` adds a
  `weftdb-stored` result to an interpolation run: the dataset is sealed into a temporary
  `SegmentStore` (1024-row segments, the narrowest lossless value type, nanosecond timestamps)
  before timing, and each rep runs `SegmentStore::interpolate_range` over it, so the gap to the
  `weftdb` result is the cost of interpolating on read from storage. The output equals the
  in-memory adapter's point for point. `SystemAdapter` gains an untimed `prepare` hook (no-op by
  default) that `run_profile` and `measure_accuracy` call once before the timed reps.
- **Block missingness and size presets in `weft-bench`** (evidence track EVI-9). `--sparse` is a
  workload of its own: a seeded grid of `--sp-series` series × `--sp-blocks` blocks of `--sp-rows`
  rows is opened cell by cell at `--sp-density` percent, held as one nullable `f64` column per
  series (an Arrow buffer plus its validity bitmap, so an absent block is absent from a column
  rather than an `Option` in a `Vec`), and timed on the block-aligned mean scan that has to visit
  every block because nothing in the column yet says which ones hold values. The report names the
  grid and the **observed** missingness rather than the density asked for — a coin flip per cell
  lands where it lands — and gates cell by cell against the generator's own per-cell means.
  `--scale tiny|small|medium|large` now expands into the size knobs of the workloads that generate
  their own data (`--sparse`, `--gap-fill`, `--downsample`), filling only the knobs left at their
  default and writing the preset into the profile name, so grids of different sizes cannot be
  compared as though they were one run.

### Added

- **One HTTP/1.1 serve path with pre-auth listener limits** (security track
  SEC-4, absorbing ROB-23's caps). All serving now goes through
  `weft_server::serve::run` (hyper's HTTP/1.1 server) instead of a bare
  `axum::serve`: a connection cap taken before `accept` (default half the
  process's `RLIMIT_NOFILE` soft limit, clamped to 16..=65,536;
  `WEFT_MAX_CONNECTIONS` overrides), a 30 s header deadline
  (`WEFT_HEADER_READ_TIMEOUT_SECS`), a 30 s keep-alive/read-idle deadline
  (`WEFT_KEEP_ALIVE_TIMEOUT_SECS`) and a 10 s write-stall timeout
  (`WEFT_WRITE_STALL_TIMEOUT_SECS`) — all enforced before any route or auth
  code runs. SIGINT/SIGTERM handlers are installed before every other
  startup phase, so a signal that arrives before serving starts is recorded
  and the server exits 0 without accepting; while serving, a signal stops
  the accept loop and live connections drain for up to 5 s.

- **The `TimeUnit` calendar policy, stated and pinned** (roadmap 4.1). A timestamp column
  holds ticks since `1970-01-01T00:00:00Z` in UTC under the POSIX rule: no zone is stored —
  the column holds instants, so a segment means the same thing whatever the writer's offset —
  and no leap second is modelled: a stored day is exactly `86_400 × nanos_per_unit()` ticks,
  which means the two moments the real calendar inserted a second are represented by counting
  that second twice rather than by a discontinuity or a renumbering. WeftDB carries no
  leap-second table, does not smear, and cannot answer "what was the TAI-UTC offset here"; a
  downsample or interpolation across a real leap second sees the interval as one second long.
  The arithmetic the policy implies moved off the call sites and onto the type:
  `TimeUnit::nanos_per_unit`, `TimeUnit::to_nanos` (checked — a coarse unit near either end of
  `i64` is refused rather than wrapped into a wrongly sorted instant) and
  `TimeUnit::from_nanos` (truncates toward minus infinity, so `-0.5 s` and `+0.5 s` no longer
  share a bucket that a full second either side misses). `scaled_rows` in
  `weftdb/src/types/segment_store.rs` now calls it instead of repeating the per-unit match.

- **Every HTTP route now registers through one sealed registry** (security track
  SEC-3). `weft-server/src/lib.rs` no longer carries the route chain: each of the
  55 routes (49 path templates) is registered in `weft-server/src/routes.rs` by a
  per-area registration function, with a `RouteSpec {scope, audit,
  operator_detail, tier, admission_class, operation_id}`. The scope table is
  frozen at eight scopes (`probe`, `metrics`, `read`, `compute`, `ingest`,
  `declare`, `maintain`, `backup`) alongside six roles with cumulative grants
  (`anonymous` through `admin`); SEC-6 will enforce the table with API keys. The
  one registry produces the router, the policy table, the admission-class table
  and a generated OpenAPI document (`routes::openapi_document()`). The gate is
  fail-closed: a request to a registered path under an unregistered method is
  refused `405 method_not_allowed` with an `Allow` header (previously axum's
  bare 405 with an empty body), and an unregistered path keeps the envelope's
  `404 route_not_found`. No route changed its method, path or handler.

- **`weft-server import-rows-mode` migrates one 0.1.x subject into a Storage v2 store**
  (legacy lane FRE-14c). A 0.1.x data directory keeps one SQLite database per aspect,
  timestamps in milliseconds, with its ingest queue, pattern stores and `metadata.db` catalog
  beside the rows; the server reads such a tree as it is and refuses to open it as a store, so
  carrying one forward meant calling `weftdb::rows_mode` from your own program. The command
  reads one subject, seals its rows into segments, prints what moved and what stays behind with
  the reason for each, and records the run in the store — a second run against the same tree
  refuses rather than doubling the rows. The migrated tree reads back under its own name
  through `WEFT_STORE_DATABASE` and `WEFT_STORE_SUBJECT`.
- **A rows-mode tree too big to hold one aspect in memory, and its catalog's non-row
  state, cross with the rows** (legacy lane FRE-14c remainder). The import no longer
  buffers an aspect: the encoding survey streams its rows through a cursor (one pass
  for the scale and the scale-free candidates, a second for the scaled candidates once
  the scale is known) and the seal streams them again in chunks of
  `--rows-per-segment`, extended past any group of rows sharing the chunk's last
  instant, so a chunk holds at most the chunk size plus one instant group. The encoding
  decided is exactly the whole-column recommendation at tolerance zero, and a tree whose
  `metadata.db` is not the catalog 0.1.x wrote is refused before anything is sealed.
  Numbered migration `0003_fre14_legacy_catalog` registers a `legacy_catalog` table in
  the store's own `metadata.db`, and the import writes one row per aspect it carried —
  the catalog's aspect row (legacy id, resolution, creation time, recorded span,
  compression config) under the store's scope. The catalog's transaction log and host
  paths stay behind, as do the dictionaries and pipeline/correlation/pattern databases:
  1.0 does not replay 0.1.x's log, and the import record in `store_meta` is the
  transaction this migration records.
- **Stage spans for every stored read, and an index-write span on the seal** (roadmap Phase 3).
  Each stored read (CSV, Arrow, Parquet, JSON, point and at-multi) now closes a
  `storage.<stage>.serialize` span tagged with its `format`, so the encode is timed apart from the
  read it follows; the Arrow and Parquet handlers split their encode out of the read to do it. A
  seal's index write closes a `control_plane.index.insert` span carrying `rows_changed`, the
  affected-row count of the insert.
- **A point read or a downsample no longer opens a segment that holds no value**
  (roadmap 4.3). Quality
  pruning is wired into the read path at both levels it applies to. The segment index prune
  drops a segment whose recorded stats say every row is null — `SegmentIndexStore::prune_present_descriptors`,
  the SQL mirror of the resident `SegmentIndex::prune_present_by_time` — so a fully null
  segment costs one index row instead of a file open, a footer read and a timestamp decode;
  inside a surviving frame the same test on the frame's own persisted segment and page stats
  drops pages a point lookup cannot answer. `weftdb::SegmentStore::read_point` and
  `read_points` are the callers, and `weft_physical_type::weftseg::read_frame_point`,
  `read_frame_points` with their single-block and paged forms do the skipping. The answers do
  not change — a null row is not a value — only the work done to reach them.
  `SegmentStore::downsample_range` prunes the same way: a row carrying no value enters no sum,
  count, min, max, first or last, and a partial holding nothing is dropped when buckets merge,
  so a window covered only by nulls yields no buckets rather than a run of empty ones — pinned
  against the single-pass `reduce` over the live rows. `read_time_range` deliberately still
  reads every time-overlapping segment: a null row is part of *its* answer, the timestamps the
  caller asked for, so pruning there would change results rather than cost.
- **Every README performance figure now has a registry entry** (evidence track EVI-2).
  `bench/claims.json` names, per figure, the bench or the local corpus behind it, the date it was
  measured, and whether a clone can re-run it; `tools/check_claims.py` runs in CI and pairs each
  entry with its `<!-- claim:id -->` marker in the README, so a figure added without an entry, a
  figure whose text has drifted away from its marker, a source path that no longer exists, or a
  `*Source:*` line no entry claims all fail the build. Eleven figures are registered: the
  point-read set and `--downsample` are marked re-runnable from a clone, the real-BTC corpus
  figures are marked as not reproducible from a clone, and one entry records a retraction rather
  than a result. No number changed.
- **The Linux release binaries start on a seven-year-old glibc** (portability track
  PAC-2). Both Linux build legs now compile inside the pinned manylinux glibc 2.28
  baseline image instead of directly on the Ubuntu 24.04 runner, so a released archive
  needs glibc 2.28 or newer rather than the 2.34 (`weft-server`, `weft-bench`) and 2.39
  (`weft-tui`) that v0.1.0's did. The floor is asserted, not assumed:
  `scripts/check-glibc.sh` reads the highest `GLIBC_*` version out of each built
  artifact's dynamic symbol table and fails the release above 2.28 — a newer reference
  ships silently and dies at `execve`, which is how this was first noticed (a `fcntl64`
  at GLIBC_2.39 inside a binary built for "Linux"). Measured in the pinned image: 2.28
  for `weft-tui` and `weft-server`, interpreter `/lib64/ld-linux-x86-64.so.2`. The two
  Linux images are now pinned by digest as well as by tag, and
  `scripts/release/test-check-glibc.sh` tests the checker on every push: it runs only in
  the release workflow, which no CI job exercises, and it had landed without its
  executable bit — the step that asserts the floor would have failed with
  "Permission denied" on the first real release.
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
- **A panic during a control-plane write ends the process** (robustness track ROB-2, release
  plan C12, C22). `weftdb::exec::control_plane_write` already marks the region that the whole
  `segment_index.db` write path runs in; the server now acts on it. `weft_server::panic_hook::install()`
  is installed at startup immediately after the tracing subscriber, chained to the hook it
  replaces so the panic is still reported with its message and location. A panic whose
  panicking task is inside that region is written to stderr and the process
  `std::process::abort()`s: the exit carries a signal a supervisor sees (SIGABRT) instead of
  an HTTP status, and the restart that follows reopens the store under the engine's own crash
  recovery, which is the component that can tell whether a commit was torn. A panic anywhere
  else is reported and left unwinding, because wave 6's ROB-3 catch layer is the only layer
  that catches panics. The hook does not write the poison marker — most of the region reads
  before it writes, so a marker there would turn a benign panic into an outage lasting until
  an operator deletes a file. `weft-server/tests/control_plane_panic.rs` proves both halves
  by re-executing the test binary as a child, the way `poison_exit.rs` does.
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
  of a control-plane write from one in a read. `poison_global()` is a process-wide
  poison that, once set, refuses every write of every store, as a store's own poison
  does, while reads go on. It refuses every store open too (`weftdb::Poisoned`), since
  an open writes.
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
- **Weft-Bench `--gpu-memory` workload**: repeated mixed-size GPU interpolations with the
  buffer pool's statistics sampled after every call, gated on bounded, non-growing idle
  memory (`--gm-rounds`, `--gm-sizes`, `--gm-knots`, `--gm-pool-mib`); reports carry a
  `gpu_pool` block (bench schema v22).
- **Weft-Bench reports the first rep apart from the rest**: `cold_warm` (bench schema v21)
  carries the first timed rep and the warm reps' latency statistics for every result with
  two or more reps; `latency` still covers every rep.
- **Weft-Bench `--ds-scaled`**: the downsample workload times the integer-native reduction a
  stored `ScaledI64` aspect takes (`reduce_partial_scaled`), gated on matching `reduce`.
- **Weft-Bench `--decimals N`** for `--synthetic` (`SyntheticParams::value_decimals`):
  rounds the generated samples to N places. The default keeps each sample's exact binary
  expansion, which interpolates ~2× slower at 100k samples than two-decimal input.
- **Weft-Bench `--parquet`**: the results as a flat Parquet table beside the JSON report, one
  row per result with the run metadata repeated, for querying many runs at once.
- **Weft-Bench records the GPU driver.** An interpolation run's `metadata.engine` gains
  `gpu_driver`, the driver's name and version as splimes' `GpuInfo::driver` reports it
  (e.g. `NVIDIA 610.57.04`), beside the adapter in `gpu`; it is printed and shown in the
  HTML report, and omitted when the driver reports nothing (bench schema v20).
- **One error envelope and one code table for every refusal** (1.0 plan X-ERR; absorbs SEC-2,
  FRE-6a, ROB-1 and TAG-0's envelope). Every refusal `weft-server` sends — a route guard, a body
  or query extractor, or a library failure behind `anyhow` — is now
  `{"error","code","request_id"}` plus optional `details` / `limit` / `committed`, as JSON on
  every path, including the ones that answered axum's empty or plain-text 400 and 404 bodies.
  The code is one of 42 named by `Code` in `weft-server/src/error.rs`, whose table is asserted by
  name and status against the plan's §4 table. Refusals are classified by **type**: `anyhow`
  downcasts find `weftdb::InvalidAspectName`, `weftdb::StoreError`,
  `weft_physical_type::SealError`, `weft_reduce::ReduceError` and `weft_reduce::SketchError`, and
  the seal wrappers in `SegmentStore` now keep the typed error as the failure's source while
  keeping their own `seal failed: …` sentence as the message, so rewording a wrapper cannot move
  a status. Two message rules remain, each labelled with the slice that retires it: `no declared
  schema` until D-S13 makes an absent schema a typed error, and `weft-arrow-store`'s untyped
  `require_sorted` bail until the ingest lane types it (D-S10/D-S14). A `5xx` now names only its
  condition — `internal`, `commit_failed`, `frame_undecodable`, `series_binding_mismatch`,
  `incompatible_store`, `poisoned`, `outcome_unknown`, … each with one fixed sentence — and its
  cause goes to the log, so no driver or library message reaches a client carrying a path, a
  table name or an aspect name (SEC-2). `axum`'s `CatchPanicLayer` is dropped: a panic is a
  process death with the exit code the supervisor sees, not a response body (FRE-6a).
  `ConvertError`, `ReduceError` and `SketchError` gained `#[non_exhaustive]`, so the mapping can
  grow without breaking a semver-minor release.
- **An aspect name is checked twice — at creation and on use** (1.0 plan X-NAME; absorbs FRE-7a,
  SEC-0). `weftdb::aspect_name::validate_new` adds the rules that only a name not in use yet can
  still be held to: no ASCII uppercase letter, because `price` and `PRICE` are one and the same
  file name on a case-insensitive or case-folding filesystem (NTFS, APFS by default, ext4 with
  `casefold`), and the legacy `<aspect>-<id>.weftseg` scheme writes the aspect name into that name
  literally — two aspects, one file, and the second one's frames are read as the first's. Only
  `POST /api/v1/storage/aspects` applies it, because declaring is where a name is born: reads,
  seals, maintenance and the store-wide sweep keep plain `validate`, so a store that already holds
  `Price` is still readable, writable and compactable. A refusal carries
  `code: "invalid_aspect_name"` and `details.reason`. The characters only Windows forbids
  (`< > : " | ? *`) are deliberately **not** refused at creation on Unix while the aspect name is
  still part of the file name — `job:rate5m` is a name in use — and that cross-platform hazard is
  closed one layer down instead, where `aspect_name::encode` escapes `:` for the write-once frame
  names (D-S10).
- **Our executables say what allocator they run on** (1.0 plan PAC-3). `weft-server` and
  `weft-tui` each declare `#[global_allocator] static ALLOCATOR: mimalloc::MiMalloc` in their own
  `main.rs`, through a direct `mimalloc` dependency rather than one inherited from a library.
  Same allocator, same version, same measured behaviour — the decision is just made in the place
  that owns the process (see the dependency entry under Changed).
- **Frame versions are a registry, and a frame says what it is** (1.0 plan X-FRAME; absorbs FRE-4,
  TAG-2 and ROB-8's fault-class naming). `weft_physical_type::frame_registry` names the frame
  layouts — `5` single-block, `6` paged, `7` envelope — on the rule stated there that a number is
  never reused or renumbered once it has shipped. The v7 envelope is a header and a trailer around
  a frame stored **verbatim**: `WEFTSEG | u16 7 | u16 body_version | extension table | frame |
  crc32`, 16 bytes of overhead (a 12-byte header plus the trailer), so a v5 or v6 body keeps every
  byte-for-byte expectation a released server already holds while a reader gains somewhere to put
  material in front of a body it is not allowed to change. An extension is `{id, critical,
  payload}`: a reader that does not know a **critical** one refuses the frame, because that body
  was written on the assumption that the extension would be applied, and an optional one is
  ignored. The set this build implements (`KNOWN_EXTENSIONS`) is deliberately empty — the envelope
  ships with no extensions, and a number invented before there is a feature behind it can never be
  taken back. `describe_frame`, `frame_layout_of`, `wrap_frame_envelope`/`unwrap_frame_envelope`
  and the layout-aware readers `read_frame_segment`, `read_frame_paged_segment`, `read_frame_point`,
  `read_frame_points`, `read_frame_range` and `read_frame_range_physical` read a frame without the
  caller knowing how it is framed, and `frame_fault_of` gives the class an operator acts on (plan
  C5): `Integrity`, the bytes are damaged, or `Undecodable { newer_format }`, this binary is behind
  the file. `SegmentStore` picks its reader from the frame's own header rather than the
  `format_version` it recorded in the index row, where any value that was not `6` used to send a
  paged frame into the single-block decoder; the decode failures it returns carry
  `weftdb::FrameFaultError` in the `anyhow` chain under the `decoding [paged ]segment <path>: …`
  sentence they have always produced. Over HTTP that is C5: a frame this build has no reader for is
  `503 incompatible_store`, and any other frame that will not decode is `500 frame_undecodable`,
  the code X-ERR gave a wire sentence but no producer. **No writer opts into the envelope yet** —
  seals keep writing 5 and 6 — so this is the mechanism, not a format change.
- **A WeftDB 0.1.x data directory is refused, not quietly emptied** (1.0 plan FRE-14, first half).
  A 0.1.x root keeps one database per aspect — `metadata.db` at the top and
  `<subject>/<aspect>/measurements.db` two levels down — and that `metadata.db` is one of
  `weftdb::CONTROL_PLANE_FILES`, so opening such a root used to **succeed**: it found a control
  plane, created the three databases it was missing, added `segments/`, wrote a `STORE_FORMAT`
  marker beside the old data, and reported an **empty** store with no error anywhere. Before any
  database is opened, `SegmentStore::open` now recognises the shape and returns
  `StoreError::RowsModeDataDirectory { aspects }`, naming each `<subject>/<aspect>` pair in path
  order. It rides X-ERR's existing mapping — `503 incompatible_store`, CLI exit 65 — so there is no
  new status code. The refusal writes nothing but `LOCK` and `LOCK.holder`: the test asserts the
  file set is otherwise unchanged, that no marker is added, and that a second open refuses the same
  way. Turning those rows into frames is the second half of FRE-14, below.
- **A WeftDB 0.1.x data directory can be read into a store** (1.0 plan FRE-14, second half).
  `weftdb::rows_mode` reads the rows-mode tree the refusal above names. `discover` reports the
  series under `<subject>/<aspect>/measurements.db` — row count, span, how many rows share an
  instant — and lists, with a reason, everything it will not carry: the `metadata.db` catalog,
  the databases 0.1.x kept beside the rows, and the `dictionaries/` directories. An aspect's name
  comes from its **directory**, not from the database: `measurements.db` holds only
  `(id, dataset_id, timestamp, value)` — 0.1.x's own DDL (`weftdb/src/types/aspect.rs` in the
  `v0.1.0` tag) — so the directory is the only record of what a series is called. The value column
  has to be the `value` that DDL created: a database that names it after the aspect is refused, not
  read, because an aspect called `dataset_id` would otherwise be read out of the column holding its
  identifiers. `import_into` reads
  one subject into the store scoped to that `(database, subject)` pair, declares each aspect under
  the narrowest encoding that holds its values exactly (tolerance zero, `recommend_encoding` — the
  reading trades no precision across the version boundary), seals the rows in batches
  (`rows_per_segment`, default `DEFAULT_ROWS_PER_SEGMENT`) without ever splitting a group of rows that
  share one instant across two segments — the store folds two segments whose windows intersect and
  keeps the newer row at an instant they share, so a group cut in two would lose rows in a compaction
  the reading does not control; a segment can therefore come out a few rows longer than asked. The
  reading then writes an `ImportRecord` into the
  store's own index under `rows_mode_import:<database>:<subject>`; `read_import_record` reads it
  back after a reopen. That record is what stops the same tree being read twice
  (`RowsModeError::AlreadyImported`), because after the fact a migrated row and a row the store
  sealed itself are indistinguishable — so an aspect that already holds segments refuses first
  (`AspectNotEmpty`), as does a tree whose database directory is not the store's database
  (`ScopeMismatch`: 0.1.x puts its database only in the path, so the path is the only thing that
  can say where a tree belongs). Timestamps are `Millis` and cross unchanged: 0.1.x stamped
  `timestamp_millis()` into an `INTEGER`, and the `resolution` its catalog records is the sampling
  grid, not a unit, so there is nothing to infer and nothing is inferred. A value cell that is not
  the text of a decimal stops the read and quotes the row (`ValueNotDecimal`) rather than putting
  a number nobody can vouch for into the store. The tree keeps its bytes: each `measurements.db`
  is opened bare, not through the control plane's `connect`, which switches journal mode and so
  writes; the test compares every file it found, byte for byte, after the reading. The aspect list is
  written one aspect at a time and its record last, so a reading that stops partway leaves the aspects
  that already crossed in the store with nothing recorded — the next attempt refuses on `AspectNotEmpty`
  rather than finishing the job, and saying what an operator does then is the remaining part of FRE-14:
  the route an operator runs from the command line or the server, the numbered migrations that register
  `metadata.db`'s schema, and a reading of a tree too big to hold one aspect in memory.
- **A checker for the metadata a release is made of** (1.0 plan FRE-1). `scripts/crates/check.py`
  reads the workspace manifests and reports, one line per problem, each state that would make a
  publish fail or ship the wrong thing: a crate marked `publish = false` that is not one of the three
  binaries the release archives, or one of those three without it; a crate that declares no version of
  its own; an `edition` a crate sets against the workspace's; an internal path dependency with no
  version requirement, or one the member's own version does not satisfy; a published crate with no
  `README.md`, no `documentation` URL, or a `license` expression whose text is not in the crate
  directory. It reads the manifests and nothing else — registry resolvability cannot be checked before
  the first publish, because `cargo package -p weft-reduce` needs `weft-physical-type` to already be on
  crates.io, which is the chicken-and-egg `scripts/publish.sh` documents — so it runs offline: in CI as
  the new `crates` job, and again inside `publish.sh` before it uploads anything. Its `publish-order`
  subcommand prints the publishable members in dependency order, topological with member order as the
  tie-break, and `publish.sh` takes both its list and its order from it rather than from an array
  someone has to remember. `scripts/crates/test_check.py` (36 tests over a throwaway four-crate
  workspace) includes the one that runs the checker against this repository.
- **Committed goldens for the bytes the store writes** (1.0 plan FRE-3). Every frame-format test in this
  repository was a round trip — encode, decode, compare against the input — which passes unchanged after a
  change that moves the writer and the reader together, so it said nothing about whether a reader from the
  previous release can still read a file this one writes. Three suites now diff against committed bytes:
  `weft-physical-type/tests/golden_frames.rs` over 20 `.weftseg` fixtures (single-block v5 across every
  reachable physical type and value codec, all five timestamp block codecs, the persisted checkpoint index,
  the paged v6 layout with its page table, the v7 envelope bare and carrying an unknown non-critical
  extension, and the bit-sliced transposed codec behind `bitsliced-codec`); `golden_codecs.rs` over 40
  encoding fixtures — 36 byte images plus four text records, because a run-length split or a
  codec-selection table is easier to review as words than as hex (the bit-packed width and payload, the
  run-length split, the delta-of-delta anchors, the zigzag varint sizing and, behind
  `experimental-codecs`, each persisted `f64` codec over the corpora that realize it plus the fixed table
  the codec-selection heuristic chooses from); and
  `weftdb/tests/golden_sidecar.rs` over 2 `.weftpart` fixtures, a base resolution and a rollup chain. Each
  case pins two things: the bytes, and the contents those bytes decode to, against a result recomputed from
  the corpus — so a fixture re-recorded after a behaviour change cannot smuggle different numbers through
  the byte pin. A mismatch reports the offset, a hex window around it, and what a reader from the last
  release would now make of the file; `WEFT_UPDATE_GOLDENS=1` re-records, and the failure text says plainly
  that re-recording is a compatibility decision rather than a test fix. An orphan check fails if a fixture
  appears in `tests/golden` that no case writes, or a case writes one that is not committed, so the fixture
  set cannot drift from the suite behind it. Gated fixtures stay on disk in a build without their gate and
  are skipped rather than deleted — their presence is the gate working. The gated halves are exercised by
  CI's `features` job, which builds `weft-physical-type --all-features` (plan note C21 asked for exactly
  `--features experimental-codecs,bitsliced-codec`, which that job's `--all-features` is a superset of).
- **An open value-provenance vocabulary** (1.0 plan TAG-3). `weftdb::ValueKind` names how a value in a
  result was produced — `raw`, `interpolated`, `extrapolated` — as a type a client can match on without
  the match becoming a breaking change later. splimes' `PointKind`, which the compute endpoints
  re-export as the JSON `kind` field today, is an exhaustive enum, so WeftDB cannot grow it in front of
  its own clients: code compiled against it today stops compiling in the release that adds a kind.
  `ValueKind` is `#[non_exhaustive]`, and it is open now rather than only in some future release — a
  token this build cannot name parses to `ValueKind::Unknown` holding the token verbatim, so a newer
  vocabulary read by an older client degrades to "a kind I have not been told about" instead of a parse
  error or a wrong arm. The wire tokens are the compatibility surface, and a test pins them to the three
  strings the endpoints already send; `RESERVED_VALUE_KIND_TOKENS` (`compressed`, `reconstructed`)
  reserves those token names before either becomes a variant, which is what lets a client special-case
  them now and still compile when they arrive. `From<PointKind>` is total (splimes' kinds are a subset),
  `TryFrom<ValueKind> for PointKind` fails with the token it could not express, and the `serde` impls
  carry the token rather than the variant. No read path emits a `ValueKind` yet: the lanes that report
  provenance adopt it where they already know it (TAG-0, FRE-5, FRE-6b, ROB-16), so until then adding
  the type costs an existing caller nothing.

- **A run manifest, so a benchmark number can be re-checked** (1.0 plan EVI-3). A Weft-Bench run wrote a
  report of numbers and a machine fingerprint, and nothing tied the two to the artifact that produced
  them: a quoted figure carried no proof of which build, which toolchain, or which file it came from, so
  a claim about a benchmark could not be audited or reproduced from the report alone. Every run now also
  writes `<report-stem>.manifest.json` beside the report (`--no-manifest` opts out): schema v23, and its
  own schema — the report artifact's schema stays at v22, because the numbers did not change shape, only
  the record around them did. It carries the toolchain facts of *the binary that ran*: `rustc` semver,
  commit and host, LLVM version, target, and the profile the code was optimized with (`profile`,
  `opt-level`, `debug`, `lto`, `debug_assertions`, `panic`, and `target-cpu` when the build set it). Those
  are baked in by a new `weft-bench/build.rs` from the environment cargo hands a build script, not read at
  run time, because by the time a manifest is written the active toolchain may be a different one and a
  released binary has no toolchain at all — a manifest that described the machine instead of the measured
  build would be evidence about the wrong thing. Alongside them: the git revision and whether the tree
  was modified, the hardware and engine fingerprint the report already carried, the allow-listed
  environment (one entry per variable, a value never read cannot leak), the command line and working
  directory, `--label name=value` operator labels, the `--claim <ID>` this run is evidence for, and a
  `grade` block naming every headline measure with its unit and the direction that is better
  (`latency.p50_ns` lower, `throughput.points_per_sec` higher, `storage.bytes_per_point` lower,
  `accuracy.rmse` lower, `correctness.passed` as 1 or 0) so a comparison has one vocabulary instead of a
  reader guessing which of two columns wins. Free text passes through a new `weft_bench::redaction`
  module — secret-shaped key names, known token prefixes, URL user-info and PEM blocks become
  `[redacted]`, `$HOME` becomes `~` — while an ordinary CPU model, file system or mount point is left
  exactly as it was, since a manifest that has eaten its own provenance is not evidence either. Capture
  refuses to write a manifest that does not verify: a credential still visible in it, a missing report, a
  digest that does not match, or a duplicated measure name. `weft-bench verify <MANIFEST>` re-checks a
  manifest later — required fields present, nothing credential-shaped left, the report found beside the
  manifest or at its recorded path, and its sha256 equal to the recorded one — prints every problem rather
  than the first, and exits non-zero; a manifest with no recorded revision, or built from a dirty tree, is
  reported but classed as advisory, so `--accept-advisories` can gate a development run on the record
  rather than on the state of the checkout.

- **A regression gate for benchmark numbers** (1.0 plan EVI-5). The harness could produce a report and a
  manifest, and nothing could answer the only question a nightly needs: is tonight slower than the last
  night we were happy with? Comparing two runs by eye is how a 12% latency walk to nowhere gets called
  noise, and how a real one gets called a bad machine. `weft-bench compare <BASELINE.json> <CANDIDATE.json>`
  pairs workloads by profile, adapter, workload and dataset shape — a changed point count or seed is a
  different workload, not a slower one — and rates each headline measure `flat`, `improved`, `suspicious`
  or `regression`, using the measure names and better-directions from the manifest's `grade` block so the
  two commands cannot disagree about which of two columns wins. A measure counts as a regression only when
  it worsens by more than `--threshold` (default 10%) **and** the two runs' bootstrap intervals do not
  overlap: a large move the noise still explains is reported as suspicious and does not fail the gate
  unless `--strict` says otherwise. A measure with no interval to begin with — throughput, storage size, a
  ratio or a count — can therefore never be a regression on its own, and `compare` says so in its note
  rather than inventing a confidence it does not have. The correctness flag is the exception: it has no
  magnitude to bound, so a workload that was correct and is not any more is a regression at any threshold.
  It refuses to compare across operating systems, architectures or report schema versions (a v22 column and
  a v23 column of the same name need not mean the same thing; `--ignore-provenance` overrides), tolerates
  the CPU, core count, memory and file system a
  hosted runner changes by itself, and treats comparing nothing at all as a failure, because a renamed
  workload would otherwise report success. It exits non-zero on a regression or a refusal; `--json` and
  `--markdown` write the same `Comparison`, so the verdict file and the exit code cannot drift apart. A
  new nightly workflow, `.github/workflows/bench-regression.yml`, builds the release profile, runs one
  fixed synthetic workload with a pinned seed, verifies its own manifest, and compares against the newest
  *successful* run of itself, so a red night stops being the baseline instead of becoming tomorrow's
  excuse; it is separate from `ci.yml` on purpose, because `scripts/release/verify.sh` reads CI as release
  evidence and a noisy night must not be able to veto a release.

### Changed

- **Reduction tokens are exact-case** (ROADMAP, FRE-5). `Aggregation::from_token` now matches the
  lowercase wire tokens exactly, so `?agg=`, the JSON body's `aggregations`, the stored-range
  `agg=` and `weft-bench --ds-aggs` refuse `Min`, `SUM` and `MEDIAN` with a `400` (or a CLI error)
  that names the token. **This breaks v0.1.0**, whose query string folded case: send `min`,
  `sum`, `p50`, `twa_linear` and the other lowercase spellings the README lists. The
  underscore spellings, the aliases and the bare lowercase names are unchanged.

- **The segment-set rollup lives in `segment_index.db`** (1.0 plan D-S11). The
  rollup row (`aspect_metadata`) was written to `metadata.db` in a second transaction
  after a seal's index commit, and rebuilt in a second transaction after a swap's —
  so a crash between the two left the rollup behind the index it summarizes, and a
  failed rebuild left it stale with only a warning. Now the seal's transaction folds
  its segment into the rollup row, and the swap's transaction writes the rollup its
  post-swap rows derive, in the same `segment_index.db` transaction that commits the
  rows. `SegmentStore::aspect_metadata`, `store_stats` and the fragmentation guard
  read the rollup from `segment_index.db`; the journal replay still re-derives it as
  a no-op reconciliation. `metadata.db` keeps the rows-mode legacy catalog, and its
  rollup table is the pre-S11 home, no longer written by the store.
- **The README stops promising what no clone can deliver.** No benchmark artifact — manifest or
  result JSON — is committed anywhere in the tree, so its Performance section now distinguishes a
  figure that names the bench it came from from one that is only a dated local measurement, and
  points the remaining gap at the claims registry (roadmap EVI-2). Documentation only; no number
  was changed or dropped.
- **`tokio` is 1.53.3, not 1.53.2.** RustSec advisory RUSTSEC-2026-0386 (`unsound`) reports that
  `AsyncFd`'s safe constructors — `new`, `with_interest`, `try_new`, `try_with_interest` — accept
  any `T: AsRawFd` rather than proof that the caller owns the descriptor, and that a descriptor
  duplicated and then closed while an `AsyncFd` is live can leave a freed registration behind in
  `epoll`'s user data, which is a use-after-free when a readiness event fires. Tokio deprecates
  those constructors in favour of `AsyncFd::register` from 1.51.6 and 1.53.3, so the fix here is a
  patch release: the lockfile moves one version and one checksum, and no API this workspace calls
  changed name or signature. Nothing in this repository constructs an `AsyncFd` — `grep -rn AsyncFd
  --include='*.rs' .` finds no match — but `weftdb` and `weft-server` both link tokio directly and
  everything else reaches it through turso, axum, hyper-util, tonic and reqwest, so every release
  archive built before this commit carried the flagged version. `deny.toml` keeps
  `[advisories] ignore = []`: an advisory that names a released fix is upgraded, not muted.

- **Every crate declares its own version, and the workspace declares the edition** (1.0 plan FRE-1).
  `[workspace.package]` carries no `version` any more and each of the ten members states
  `version = "0.1.0"` where it inherited one, because a release is a statement about specific crates
  and a patch of one library is not a reason to put a number on nine that did not change. The release
  pipeline is unchanged: `weft-server`'s version is still the one a tag names, which is what
  `scripts/release/verify.sh` reads and refuses a disagreeing tag about. `edition` went the other way —
  the workspace states `edition = "2021"` and every member inherits it, so FRE-22's move to edition
  2024 is one edit in one file instead of ten in ten. The internal `[workspace.dependencies]` path
  entries keep their explicit `version = "0.1.0"` requirements, and `scripts/crates/check.py check`
  (see Added) is what keeps those requirements and the members' versions in step. Each published crate
  now states `documentation = "https://docs.rs/<crate>"`, and `weftdb` carries a `README.md` of its
  own: it inherited the repository's, so its crates.io page would have described a server and a
  terminal UI on a library's page.
- **Library crates stop deciding their users' allocator and GPU stack** (1.0 plan PAC-3). No
  library in this workspace enables turso's `mimalloc` feature or splimes' default `gpu` feature
  any more, because both are decisions a dependency makes for the whole process. turso 0.8.1
  installs a `#[global_allocator]` in its own crate root when `mimalloc` is on, so every program
  linking `weftdb` — an embedder with its own allocator, a program that wants the system one — was
  switched to mimalloc without being asked; `gpu` drags `wgpu`, and with it the Vulkan, Metal and
  DX12 backends, into the graph of everything that depends on splimes. Counting the distinct
  packages in each crate's non-dev dependency graph, before and after: `weft-line-protocol` and
  `weft-reduce` 77 → 25, `weftdb` 266 → 227, `weft-orchestration` 267 → 231. Our own GPU users —
  `weft-server` and `weft-bench`, which call `prewarm_gpu`, `calibrate` and `gpu_pool_stats` —
  enable `gpu` themselves, as do `weftdb`'s interpolation benches through a dev-dependency, so
  nothing this repository runs lost its GPU path. A library user who wants either asks for it: an
  allocator by declaring one in their own binary, the GPU by enabling `splimes/gpu`. ⚠️ Rust code
  that reached the GPU *through* `weftdb` — a `splimes::Backend::Gpu` while depending on `weftdb`
  alone — names `splimes` itself now. One default stays on deliberately: splimes' `serde`, because
  `weftdb::BatchedMeasurement` derives `Serialize`/`Deserialize` over a `splimes::Resolution` field.
  turso's other default, `fts`, stays off, as it already was: it is turso's full-text search, and
  turning it on grows the graph with tantivy, tantivy-columnar, zstd-sys and the rest of that tree,
  which the first attempt at this dependency line showed in the lock diff.
  One number to be aware of: `cargo bench -p weftdb` is criterion, not test code, so it used to
  inherit mimalloc through turso and now runs on the system allocator — a `weftdb` bench figure
  from before this release is not comparable with one taken after. `weft-bench` never linked
  `weftdb` and never had mimalloc, so the Weft-Bench series, including the nightly regression
  track, does not move.
- **`tracing-subscriber` is a dev-dependency of `weftdb`, not a dependency.** No code in the
  library installs a subscriber outside `#[cfg(test)]`, so every program linking `weftdb` was
  building registry, fmt and env-filter code it never called.

- **Statuses split where the §4 table names a different condition** (1.0 plan X-ERR). A batch
  that will not seal under an aspect's declared schema no longer answers `400` whatever the
  reason: a length mismatch between the two columns, a zero `rows_per_page`, and a backwards
  timestamp stay `400` (the batch itself is malformed), while a value the declared encoding
  cannot hold — an overflow, or a loss beyond the declared `value_tolerance` — is `422
  value_not_representable` with `details.max_abs_error` and `details.tolerance`, the same call
  `/api/v1/interpolate` already made for a value outside the physical type's range; a value with
  no finite image is `400 not_finite`, with the row in `details.row`. A name the store cannot
  address as a frame file is `400 invalid_aspect_name` carrying `details.aspect` and
  `details.reason`, at declare as well as on use. A store root this binary refuses to open is
  `503 incompatible_store`, the HTTP shape of the CLI's exit `65`. `POST /api/v1/downsample`
  answers `422 value_not_representable` for a timestamp that cannot be bucketed at the requested
  resolution, and `400 not_finite` for a value with no finite `f64` image, where both used to be
  reported as `500`.
- **Time-weighted averages run on integers** (`weft-reduce`): `twa`, `twa_linear` and
  `twa_bucket_end` over samples with `i64` mantissas at scales up to 17 compute the weighted sum
  in `i128` and divide once with the integer long division `avg` uses, reproducing the `BigDecimal`
  result in value and representation (including `bigdecimal`'s special cases for a value of one
  and for zeros); other samples take the `BigDecimal` loop as before. Hourly `avg,p99,twa` over 1M
  real BTC closes: 150 → 55–58 ms on points, 119 → 38 ms on stored mantissas; per stored segment
  (decode included) 155 → 79 ms.
- **Integer-path reductions spend less per bucket** (`weft-reduce`): `reduce_scaled` and
  `reduce_partial_scaled` close time-ordered buckets into a `Vec` instead of a map insert per
  bucket (out-of-order input still merges through the map), `reduce_scaled` builds a bucket's
  `sum` decimal only when `sum` is requested, and `reduce_scaled` and `PartialReduction::finish`
  size their output once. `reduce` and `reduce_partial` on `BigDecimal` points do the same, and
  skip rescaling when every value already has the series' scale. Results are unchanged. Hourly
  `avg` over 1M real BTC closes went from ~3.1× an `f64` loop to ~2.1–2.2× on stored mantissas
  (4.26–4.61 ms against 2.06–2.07 ms in the same runs), and from 24.1 to 21.0 ms (~10×) on points.
- **Exact percentiles (`p50`/`p90`/`p95`/`p99`) no longer clone and sort every bucket**
  (`weft-reduce`). A single percentile selects its rank in linear time over borrowed values
  and several share one sort; only the result is cloned. Results are identical, including
  which of two numerically equal values (`1.0`, `1.00`) is returned. A lone hourly `p99`
  runs ~1.6–3.4× faster on the bench (two-decimal, real BTC and full-expansion series),
  which made it about twice as fast as `sketch_p99` until the next entry.
- **`SegmentStore::interpolate_range`** (`weftdb`): interpolate-on-read over stored segments
  — the aspect's samples in a window (plus a `margin` of knots past each edge) run through a
  caller-supplied splimes `Interpolator`, returning the grid with provenance;
  `interpolate_range_f64` returns `f64` values (~3× faster at 1M points, since building a
  `BigDecimal` per output point dominates). A read window holding more than 4 Mi stored rows
  (`interpolate_range::MAX_KNOTS`) is refused. Library only; no HTTP endpoint yet.
- **`weft_reduce::decimal_to_f64`**: a `BigDecimal` to `f64` conversion bit-identical to
  `bigdecimal`'s `to_f64`, fast for values of at most 15–16 significant digits.
- **Stored-range percentile and time-weighted-average downsamples take the integer path**
  (`weft-reduce`): `reduce_partial_scaled` serves `p*` and `twa*` too, collecting samples
  from the stored mantissas, so `GET …/storage/{aspect}/downsample?agg=p99,twa` on a
  `ScaledI64` aspect no longer decodes each segment twice. Results are unchanged; ~1.4× per
  segment on the bench.
- **`reduce` and `reduce_partial` compute on integers when the values allow it**
  (`weft-reduce`): when every value fits an `i64` mantissa at the series' largest scale, the
  reduction compares and sums integers and returns the same buckets, value for value and in
  representation. ~2.3× on real price series, ~4× on two-decimal ones.
- **Bucket averages that never terminate are computed in one step** (`weft-reduce`): their
  100 significant digits come from one big-integer multiplication and one division by the
  count instead of a digit loop, with an identical result. Hourly `avg` on the integer
  path runs ~1.7× faster (median 15.3 → 9.0 ms per 1M rows), and every `avg` gains from it.
- **`sketch_p*` reductions skip the logarithm for a value in the previous value's bucket**
  (`weft-reduce`), with identical sketches; ~1.3–1.4× on real price series.
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

- **The JSON body's `aggregations` accepts the documented reduction spellings.** `POST /api/v1/downsample`
  read its tokens through the bare lowercase variant names only, so the README's `twa_linear`, `sketch_p99` and aliases such as
  `median` were rejected as unknown variants, and a name copied from a response (`twa_linear`) could not be sent back. The body
  now reads the query string's parser (`Aggregation::from_token`), so both surfaces accept the same set; the bare names
  (`twalinear`, `sketchp99`) still parse. An unknown token is a `400` that names it and lists the accepted spellings (ROADMAP, FRE-5).

- **A pre-S11 store read zero metadata until its next seal or swap (D-S11).**
  Moving the rollup to `segment_index.db` left stores written before S11 with no
  rollup rows at all, and the journal replay only re-derives the aspects its
  journal names — so a clean pre-S11 store opened with `aspect_metadata`
  reading default zeros (the v0.1.0 compat gate caught it: a migrated aspect
  read 0 nulls against the 14 v0.1.0 counted). Open now backfills exactly the
  missing rows after the journal replay; S11-native stores, whose rows exist,
  pay one `SELECT` per aspect and nothing more.

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
- **A downsample served from `.weftpart` sidecars could lose a zero's decimal scale.**
  With a partial-sidecar policy on, a zero `min`, `max`, `sum`, `avg`, `first` or
  `last` came back at scale 0 (`0`) where decoding the segment gives it the column's
  scale (`0.000` on a `ScaledI64 { scale: 3 }` aspect). The values were equal, but
  exact-decimal output differed. `bigdecimal` writes a decimal as its display string,
  which loses the scale of a zero and of a decimal with a small negative scale (`5E+2`
  is written `500`), so the sidecar lost it. A `PartialReduction` now writes those
  decimals in scientific form (`0e-3`, `5e2`) and every other one as before. The frame
  layout and version (v3) are unchanged, and a `bigdecimal` reader, 0.1.0's included,
  parses the new form exactly. A sidecar 0.1.0 wrote for a `ScaledI64` or `ScaledI128`
  segment gets the segment's scale back on its zeros when it is loaded. One it wrote for
  a segment of another physical type still reads back with those scales lost: nothing
  rebuilds it unless maintenance rewrites the segment.
- **A checked-out v0.1.0 fixture was not the tree the release wrote.** Rows-mode gave every
  aspect an empty `dictionaries/` directory beside its `measurements.db`
  (`v0.1.0:weftdb/src/types/aspect.rs:252`), and an empty directory is the one artifact git
  cannot store. `MANIFEST.sha256` hashes files, so it names none of them and reports no gap
  when they are absent: the 109 files came back complete from a fresh checkout — the negation
  that recovered what `.gitignore` ate accounts for every one — and the three directories did
  not. The readable leg of the rows-mode work names what it leaves behind, and one path it must
  name is `plant/price/dictionaries`, so `weftdb/tests/compat_v010.rs` failed on all three CI
  platforms and passed on any machine that had run the generator: evidence that depended on the
  machine reading it. The test now restores the directories on the copy it reads, one beside
  every `measurements.db` it finds, derived from the tree rather than from a list of aspects —
  so a fixture that grows an aspect grows its directory without anyone editing the test, and the
  committed tree stays exactly the bytes the release wrote.

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
