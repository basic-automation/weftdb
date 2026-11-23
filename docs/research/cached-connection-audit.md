# Cached-Connection Migration Audit

**Date:** 2026-11-22
**Branch:** `b-conncache/audit` (`origin/main` at `b6794c40baf16f3d92e8521dbfc2c03f102632f1`)
**Scope:** Where WeftDB opens a Turso/libSQL database handle (a `turso::Database`), whether
that handle goes through a process-wide or per-struct cache, and what would be needed to
complete the migration the roadmap calls "cached-connection" (ROADMAP.md `B-conncache`).

**Verdict: the roadmap line means "make every place that opens a Turso Database go through
one process-wide cache, deduplicating handles by path, and close idle ones to bound RSS."**
The core infrastructure (`CONNECTION_DATABASES`, `close_idle_databases`) already exists.
The SegmentStore's control-plane databases and the rows-mode reader leg still open
individual handles through `Builder::new_local` directly.  Seven independent
`get_*_db`-style methods in `Database` each maintain their own cache key in the
per-Database `DatabaseCache`, but they all terminate in `get_or_create_turso_database`
which does deduplicate.  The SegmentStore cannot borrow its DB handles from
`CONNECTION_DATABASES` without either owning them (which it already does by value) or
forcing the cache to hold them indefinitely (defeating the idle-close purpose).

---

## Vocabulary

The codebase has **two caching layers** for Turso/libSQL database handles:

| Layer | Location | Key | Purpose |
|-------|----------|-----|---------|
| 1. Process-wide | `CONNECTION_DATABASES` (`mod.rs:24`) | file path (`String`) | Deduplicate `turso::Database` handles by path; close idle ones |
| 2. Per-`Database` | `Database::cache` (`mod.rs:143`) | aspect-id-based cache key | Avoid re-reading the aspect to find its database path |

There is no `ConnectionPool`, `bb8`, `r2d2`, or `Pool` struct.  The cache holds
`turso::Database` handles (Turso's own connection pool is inside the handle, shared
by inode in the process).  There is no `CachedConnection` or `cached_connection`
in the code — the roadmap line's vocabulary does not match the implementation.

---

## Cache architecture in place

### Layer 1: `CONNECTION_DATABASES` (process-wide)

`weftdb/src/types/database/mod.rs:24`:
```rust
static CONNECTION_DATABASES: LazyLock<Arc<Mutex<HashMap<String, turso::Database>>>>;
```

Backed by:
- `LAST_USED` (`mod.rs:28`) — `Instant` per path, written by `touch_cached`
- `OPENED_ONCE` (`mod.rs` earlier, not shown here) — guards log-sweep idempotency

Entry points:
- `Database::get_turso_database` (`mod.rs:221`) — cold path, creates if the file exists
- `Database::get_or_create_turso_database` (`mod.rs:317`) — hot path, creates if absent
- `close_idle_databases(after)` (`mod.rs:122`) — closes handles unused for `after`
- `open_database_count()` (`mod.rs:128`) — diagnostics
- `clear_connection_cache_by_name(name)` (`mod.rs:166`) — test cleanup

### Layer 2: `DatabaseCache` (per-struct)

`weftdb/src/types/database/mod.rs:143` (`Database::cache`):

Each `Database` struct holds its own `DatabaseCache` (LRU with an `Object` trait).
Every `get_*_db` method in `Database` (measurements, batches, patterns, events,
correlations, dictionaries) first checks this cache, then falls through to the
aspect handle (which calls `get_or_create_turso_database` for the actual open):

- `get_measurement_db` (`mod.rs:1360`)
- `get_unprocessed_batches_db` (`mod.rs:1392`)
- `get_processed_batches_db` (`mod.rs:1425`)
- `get_patterns_db` (`mod.rs:1453`)
- `get_events_db` (`mod.rs:1477`)
- `get_unprocessed_events_db` (`mod.rs:1499`)
- `get_processed_events_db` (`mod.rs:1524`)
- `get_correlations_db` (`mod.rs:1546`)
- `get_dictionary_db` (`mod.rs:1568`)

Every one of these drops the cache handle after retrieving the value
(`aspect.rs:146–226`):
```rust
// Handles live in the process-wide cache only, so idle ones can close.
let measurements: Option<TursoDatabase> = { drop(measurements); None };
```

This is the design: the per-aspect DB lives in `CONNECTION_DATABASES` and the per-`Database`
cache only avoids re-reading the aspect configuration.  The local handle is dropped so the
process-wide cache is the sole owner and can evict it later.

---

## Call-site classification

### (A) Uses the cached path

Every call through `Database::get_turso_database` or `Database::get_or_create_turso_database`
goes through `CONNECTION_DATABASES`.  These serve the *hot* paths (ingest, query) of the
legacy 0.1.x `Database` API.

#### Hot (ingest/write, query/read)

| Site | File | Line | What it opens |
|------|------|------|---------------|
| `create_turso_database` | `mod.rs` | 208 | Delegates to `get_or_create_turso_database` |
| `get_turso_database` | `mod.rs` | 221 | Reads one by path (cold path) |
| `get_or_create_turso_database` | `mod.rs` | 317 | Creates or reuses (hot path) |
| `get_measurement_db` | `mod.rs` | 1360 | Cached via `DatabaseCache` → `get_or_create_turso_database` |
| `get_measurement_db_path` | `mod.rs` | 1378 | Path-only cache |
| `get_unprocessed_batches_db` | `mod.rs` | 1392 | Same pattern |
| `get_processed_batches_db` | `mod.rs` | 1425 | Same pattern |
| `get_patterns_db` | `mod.rs` | 1453 | Same pattern |
| `get_events_db` | `mod.rs` | 1477 | Same pattern |
| `get_unprocessed_events_db` | `mod.rs` | 1499 | Same pattern |
| `get_processed_events_db` | `mod.rs` | 1524 | Same pattern |
| `get_correlations_db` | `mod.rs` | 1546 | Same pattern |
| `get_dictionary_db` | `mod.rs` | 1568 | Same pattern |
| `initialize_aspect_stores` | `aspect.rs` | 132–226 | `get_or_create_turso_database` per sub-database |

#### Cold (admin, lifecycle)

| Site | File | Line | What it opens |
|------|------|------|---------------|
| `close_idle_databases` | `mod.rs` | 122 | Sweeps `CONNECTION_DATABASES` |
| `clear_connection_cache_by_name` | `mod.rs` | 166 | Test / admin cleanup |
| `open_database_count` | `mod.rs` | 128 | Diagnostic |

---

### (B) Bypasses the cache but could take it mechanically

These call sites open a `turso::Database` through `Builder::new_local` directly.  They
*are one-shot or lifecycle-bound opens that already try to share within the process*, but
they do not go through `CONNECTION_DATABASES`.  The handle either replaces one in the
cache (SegmentStore control-plane) or is opened once and dropped (backup verify).

#### Hot (lifetime bound)

**SegmentStore control-plane databases** — each open through `open_unmigrated` → `Builder::new_local`:

| Site | File | Line | Database File |
|------|------|------|---------------|
| `CatalogStore::open_unmigrated` | `catalog.rs` | 64 | `catalog.db` |
| `AspectCatalog::open_unmigrated` | `aspect_catalog.rs` | 73 | `aspect_catalog.db` |
| `AspectMetadataStore::open_unmigrated` | `metadata.rs` | 164 | `metadata.db` |
| `SegmentIndexStore::open_unmigrated` | `segment_index.rs` | 221 | `segment_index.db` |

These are called once by `ControlPlaneDbs::open` (`segment_store.rs:2088`) during
`SegmentStore::open_on` (`open.rs:210`).  The handles are held for the store's lifetime
and dropped when the store is dropped.  **Could** be stored in `CONNECTION_DATABASES`,
but the SegmentStore already holds them by value; putting them in the process-wide cache
would mean the store inc-refs them and the cache cannot evict them while the store lives.
Since the store is the sole long-term holder of each of the four control-plane files, the
cache would not deduplicate anything — the SegmentStore fields *are* the primary reference.

**In practice this is not a gap to close, because there is only ever one open of each
control-plane file per SegmentStore root, and the SegmentStore is the only thing that
opens them.**  The cache adds zero hits here.

#### Cold (backup verify)

| Site | File | Line | What it opens |
|------|------|------|---------------|
| `verify_snapshot` | `backup.rs` | 265 | Reopens a snapshot copy to verify it |
| `verify_against_source` | `backup.rs` | 351 | Reopens a snapshot copy to compare rows |

These are one-shot, read-only, and the snapshot file is typically a temp file that
will be deleted.  There is no value in caching them.

---

### (C) Bypasses the cache and cannot without a design decision

| Site | File | Line | Why it cannot take the cache |
|------|------|------|------------------------------|
| `rows_mode::open(database, role)` | `rows_mode.rs` | 720 | **Intentionally bare.** The readable leg reads a tree it does not own. The 0.1.x format databases may not have MVCC enabled, and opening them through `enable_mvcc_full` would write to them, violating the read-only promise. The module doc (line 10, `the readable leg reads a tree it does not own`) makes this a hard constraint. |
| `hold_without_mvcc` | `segment_store.rs` | 6233 | **Intentionally different mode.** Opens with `experimental_multiprocess_wal(true)` — the opposite of MVCC, needed by the test to pin a database out of MVCC. |
| Test code writing 0.1.x-format databases | `rows_mode.rs` | 1123, 1159, 1177, 1205, 1638 | Test helpers building 0.1.x schema. Deliberately do not go through the cache. |
| Test code opening raw databases | `segment_index.rs` | 898, 1012 | Test helpers that open the database to inspect its state after an operation. |
| Test code opening raw databases | `segment_store.rs` | 7193, 7448, 7596, 7611, 7626, 7699 | Test helpers that inspect raw `store_meta`, journal mode, or tables. |
| Test code opening raw databases | `control_plane.rs` | 229, 235, 246, 276, 312, 319, 346, 386 | Test helpers proving MVCC switch behaviour, sync behaviour. |
| Test seed_db / copy_db | `backup.rs` | 1142, 1188, 1275, 1368, 1377 | Test helpers creating databases for backup verification. |

---

## Correctness couplings

Before proposing that any new site goes through `CONNECTION_DATABASES`, the following constraints must hold:

### 1. `Drop`-based flush

The `CONNECTION_DATABASES` map holds `turso::Database` handles by value.  Dropping the
last clone closes the database (Turso uses the process-internal inode registry to share).
The cache's eviction (`sweep_idle`, `close_idle_databases`) relies on the handle's `Drop`
to release resources.  Any site that borrows a handle from the cache but keeps a clone
beyond the cache's eviction window risks the handle being dropped and reused while a
livelong transaction is still in-flight.

### 2. Durability / fsync ordering (`control_plane.rs` module doc)

Every control-plane write is a `BEGIN CONCURRENT` transaction, and every control-plane
open runs `enable_mvcc_full` which syncs the MVCC header.  If an *existing* handle from
`CONNECTION_DATABASES` is reused, its MVCC configuration was already set on first open.
This is fine: `enable_mvcc_full` is run exactly once per file by `get_turso_database`
(line ~279) — the pragma is persistent.  But the `synchronous=FULL` setting is
per-connection, and every write opens a fresh connection from the cached `Database`
handle, so the setting is the default (`FULL`).

**Caveat:** `get_turso_database` (`mod.rs:221`) sets `PRAGMA busy_timeout = 30000` on a
throwaway connection (`mod.rs:277`).  The default `turso::Database` timeout is `busy_timeout`
(opt-in, not set by Turso).  Every control-plane write creates a fresh connection which
inherits the default timeout (0 = immediate failure on lock).  The SegmentStore's `connect`
(`control_plane.rs:57`) does NOT set a busy timeout — it relies on the caller to `BEGIN
CONCURRENT` and retry on `Busy`.  This is correct by design.

**Relevance to cache migration:** if a new database were added to `CONNECTION_DATABASES`,
the configuration in `get_turso_database`/`get_or_create_turso_database` (MVCC pragma,
busy timeout) must match what the existing SegmentStore open path does, or the two paths
cannot share a cache.

### 3. Compaction / checkpoint

Not applicable to Turso handles: compaction is a Turso-internal mechanism (checkpoint of
MVCC log into main file).  No part of WeftDB calls `PRAGMA wal_checkpoint` on a control-plane
database.

### 4. SegmentStore borrow-vs-move rule

The SegmentStore holds its four control-plane `turso::Database` handles by value
(`segment_store.rs:1804–1814`):
```rust
index: SegmentIndexStore,     // holds db: turso::Database
metadata: AspectMetadataStore, // holds db: turso::Database
catalog: AspectCatalog,       // holds db: turso::Database
registry: CatalogStore,       // holds db: turso::Database
```

These are never borrowed from a cache.  Every `open_unmigrated` creates a fresh handle
that the store owns.  **If these were to go through `CONNECTION_DATABASES`, the store
would need to either (a) clone the cached handle and keep it alive, defeating the cache's
ability to evict it, or (b) take ownership out of the cache, making the cache miss on the
next open.**  Neither is an improvement over the current design where each handle is the
store's own.

### 5. Maintenance exclusivity

The `Reaper` (`segment_store.rs:1916`) holds the `segments/` directory syncer and the
frame-unlinking background work.  It does not depend on the Turso handle cache; it only
interacts with the filesystem.  No change to the connection cache would affect it.

---

## What remains

The roadmap item B-conncache lists at `docs/roadmap-notes.md:459` as *(Phase 4/7 · partial)*.
The notes tie it to the "connection reuse" work the `p12/fanctl-correlation-growth` owner
is doing (`roadmap-notes.md:768`).

What is **done**:
- `CONNECTION_DATABASES` process-wide cache exists and is the only path for the 0.1.x
  `Database` API's sub-aspect databases (measurements, batches, patterns, events,
  correlations, dictionaries).
- `close_idle_databases()` can close handles that have not been used in a configurable
  window, bounding per-aspect RSS.
- SegmentStore's control-plane databases are held by value for the store's lifetime, which
  is the right ownership model for a singleton-per-root design.

What **remains open**:

1. **`rows_mode.rs` reader opens.** Filed as #871.  Each call to `rows_mode::open()` (`rows_mode.rs:720`)
   opens a fresh Turso connection to a legacy database.  Every `import-into` run and each
   discover-skip creates at least one such open.  These cannot go through
   `CONNECTION_DATABASES` as-is because the reader leg opens the database without MVCC
   (`open` does not call `enable_mvcc_full`).  A `reuse_read_only_connections` cache
   could be built, but it would be a separate cache — never `CONNECTION_DATABASES` which
   sets MVCC on first open.

2. **Multiple `Database` struct instances for the same physical database.** Filed as #873.  Each
   `Database` struct has its own `DatabaseCache` and its own `metadata` handle (the
   top-level Turso database for catalog/subjects).  Two `Database` handles to the same
   path do deduplicate in `CONNECTION_DATABASES` for the sub-aspect files, but
   `DATABASES` (`mod.rs:21`, the `HashMap<DatabaseId, DatabaseInfo>`) is a separate
   map.  There is currently no named database singleton at the application level.

3. **SegmentStore was never intended to share its handles.** No issue was filed for this: the sequencing table rejects the work.  The store is a singleton per
   root (Locks enforce exclusivity).  The four control-plane databases of one store are
   opened exactly once and live for the store's lifetime.  Making them pass through
   `CONNECTION_DATABASES` would be harmless but not useful — no deduplication, same
   lifecycle.

4. **No idle-timeout on SegmentStore handles.** No issue was filed: this is a per-root cost already bounded by the one-store-per-process rule.  The store's four databases are always
   open while the store lives.  If the number of SegmentStore roots grows (currently one
   process opens exactly one store, through `open_from_env`), each root adds four
   databases that cannot be closed while the store lives.  This is not a cache problem
   but a per-root cost that is already bounded by design.

---

## Sequencing and cost

| Priority | Change | Effort | Risk |
|----------|--------|--------|------|
| P1 | Build a `READ_ONLY_CONNECTION_CACHE` for rows-mode opens (#871) | Small (<50 lines) | Low — `rows_mode::open` always creates a fresh `turso::Database` then `.connect()`. The `connect()` is the only allocation after the handle. A simple `HashMap<String, Weak<turso::Connection>>` with per-path LRU eviction would avoid re-probing the same legacy file on repeated import attempts. |
| P2 | Seed `close_idle_databases` into the server's background tick (#872) | Small (config + call) | Low — the function exists and is tested; it is just not called from the main server loop. |
| P3 | Consolidate `Database` struct into a process-wide singleton per path (#873) | Medium | Medium — the `Database` is seeded from `database/mod.rs` and most call sites use `database: Database` references that would need a `&Database` facade or an `Arc<Database>`. |
| — | Move SegmentStore control-plane opens through `CONNECTION_DATABASES` | Not recommended | Costs complexity, adds no deduplication, and would prevent idle-close of those handles while the store lives. |


Do not start P1 or P2 without checking the lane first: `docs/roadmap-notes.md:767` records that
the owner's `p12/fanctl-correlation-growth` lane is already working on "per-row memory probe,
connection reuse", and the notes deliberately added no roadmap line for the RSS question "to
avoid claiming it". #872 in particular is that lane's work in the shape the roadmap would
describe; it is filed so the gap is on the board, not so this loop can take it.
---

## Method note

This audit was conducted entirely by structural reading (grep and file inspection) on
the worktree at the commit named above.  No code was compiled or run.  Line numbers
were verified against `origin/main` at that commit by grepping the worktree files
directly.  Every `path:line` pair was opened and read — none is copied from a previous
report.  The analysis records structure (which functions call which, which cache paths
exist, which open paths bypass them), not dynamic behaviour.
