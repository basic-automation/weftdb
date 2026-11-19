# UPGRADING

This document covers upgrading a WeftDB deployment from one minor release to the next,
the rollback procedure, and what is and is not guaranteed about backward and forward
compatibility between releases. Operators should read this before changing the WeftDB
binary version in any environment that holds data.

The authoritative reference for the store format is
[`weftdb/src/types/store_format.rs`](weftdb/src/types/store_format.rs); the
release pins that every compatibility gate checks are in
[`compat/releases.toml`](compat/releases.toml).

---

## Duplicate timestamps on seal-backed aspects (#802, SEAL-DUP)

Aspect storage mode differences: **rows mode** stores every row at every timestamp; a
range read returns all of them. **Seal-backed aspects** at a shared timestamp keep only
the newest call's rows (duplicates inside one call are all kept). This is a behaviour
change for any aspect that becomes seal-backed, because the rows mode they replaced
appended. A migrating deployment whose workload depends on reading back multiple rows at
the same timestamp must pin `WEFT_LEGACY_DEFAULT_STORAGE=rows` to stay in rows mode.

---

## Layout 1 → layout 2 (v0.1.0 → v0.2.0)

Every WeftDB release since v0.2.0 writes store **layout 2**. A layout 1 store (written
by v0.1.0) is **upgraded in place** on the first open by a layout-2 binary: no dump
and reload is required.

### What the code does

The open procedure in `store_format.rs` (the `read_marker` and `write_marker` functions,
and the open path in `segment_store/open.rs`) works as follows:

1. The open reads the `STORE_FORMAT` marker JSON file beside the control-plane
   databases. For a layout-1 store that predates markers, `read_marker` returns `None`
   (the file does not exist).
2. The open then probes for a legacy v0.1.0 tree: if `metadata.db` exists and the root
   has no marker, the open checks whether subdirectories hold `measurements.db` files
   (the 0.1.x rows-mode shape). A store with `segment_index.db` and a `segments/`
   directory passes this probe cleanly and is recognised as a layout-1 segment store.
   (`rows_mode_aspects` at line 201 in the source returns an empty list for it.)
3. The open constructs a `StoreFormat::legacy(scope)` representation: layout 1,
   `min_read_layout: 1`, `min_write_layout: 1`, and writes the marker through
   `write_marker` (temporary file, `sync_all`, rename over the marker, fsync root).
4. The first open then runs the registered migrations (`0006_tag5_series`,
   `0007_series_time_index`, `0008_upsert_keys`) — additive DDL that adds tables
   and columns to `segment_index.db` without rewriting existing data. The marker's
   `layout_version` and floors are raised to 2, and `applied_through` records the
   newest migration id.
5. After migration, `min_write_layout` is 2. A WeftDB whose [`SUPPORTED_LAYOUT`] is
   less than `min_write_layout` refuses the store with
   [`StoreError::IncompatibleLayout`] **before it opens any database**. This is the
   primary guard: a pre-1.0 build that knows only layout 1 cannot write a migrated
   store because its `SUPPORTED_LAYOUT` is 1, which is less than `min_write_layout`
   (2).

The upgrade is deliberately **not reversible**: the marker, the new columns and the
persistent id allocator commit state are all forward-only. See the rollback section
below.

### What an operator must do

1. Stop the v0.1.0 `weft-server`.
2. **Copy the whole store directory.** A `POST /api/v1/storage/backup` copies only the
   control-plane databases, not the segment frames. A whole-store copy (cp -a or rsync)
   preserves everything.
3. Start the v0.2.0 (or later) `weft-server` against the same `WEFT_SEGMENT_STORE_ROOT`.
   The first open performs the migration automatically.
4. Verify the server starts and data reads back. The server output logs each migration
   as it applies.

### What happens to a layout-2 store opened by an older binary

What continuous integration does and does not do with a released binary is worth stating
here, because it bounds what the version table above has actually been measured against. The
`pinned release archives` job in `.github/workflows/ci.yml` downloads the v0.1.0 archive for
the runner's platform with `python3 scripts/compat/releases.py fetch --dest <dir> --extract`
and fails if the bytes hash to anything other than the sha256 recorded in
`compat/releases.toml`; it then checks that the archive holds a `weft-server`. It never runs
that binary, and no job yet points an old binary at a store the candidate wrote — the
version table's rows about older binaries are therefore reasoned from the marker protocol,
not executed. One measured fact bears on any attempt to do that: the v0.1.0 `weft-server`
takes no `--version` and no `--help`. Asked for either, it prints its configuration banner
and starts listening. A leg that wants to talk to it must set its environment and speak HTTP.

A v0.1.0 binary does not know about the `STORE_FORMAT` marker at all: it never reads it,
so it would open a migrated store without refusal. **Do not run v0.1.0 on a store that
a newer binary has opened.** The v0.1.0 binary writes frames in the old v1 frame format
and knows nothing about the persistent id allocator, the new columns, or the journaled
frame writes. Running v0.1.0 on a migrated store can corrupt it.

A v0.2.0 or v0.2.1 binary shares layout 2. It reads the marker, sees `layout_version: 2`,
`min_write_layout: 2`, and because its own `SUPPORTED_LAYOUT` is 2, opens normally.
Unknown columns are ignored, and the marker's `applied_through` field is a string that
newer binaries wrote — v0.2.x ignores it.

---

## Rollback after writes

After a newer WeftDB has opened and written to a store, can an older binary still open
it?

**Layout 2 (v0.2.x and v0.3.x):** A layout-2 store written by v0.3.0 can be opened by
v0.2.1 — the layout number is the same (2), and v0.2.1 ignores columns it does not know.
However, a v0.3.0 store may carry data in aspects that use tagged series (TAG-11), and
the older binary's read methods that do not accept a series selector answer the untagged
series only. This is not a data-loss risk (the data is still on disk) but reads will
not see the tagged rows.

**Rollback from layout 2 to layout 1:** Not supported. A store whose `STORE_FORMAT`
marker records `layout_version: 2` is refused by any build whose `SUPPORTED_LAYOUT`
is 1. No such build exists after v0.1.0.

**What a rollback attempt does — refuses, loses nothing.** The store's
`STORE_FORMAT` marker is read before any database is opened. When the marker's
`min_write_layout` is above the opening build's [`SUPPORTED_LAYOUT`], the open
fails with [`StoreError::IncompatibleLayout`] and **nothing is changed** — only
`LOCK` and `LOCK.holder` are touched. The refusal is the same whether the store
was written by a genuine newer binary or the marker has been tampered; the store
itself is not damaged, and a subsequent open with the correct binary reads every
row the older binary could not.

This contract is pinned in `weftdb/tests/compat_v010.rs`
([`v010_compat_fre13_rollback_after_writes_loses_nothing`]) which:
1. Opens the v0.1.0 fixture, upgrades it to layout 2, writes new data, and closes.
2. Tampers the `STORE_FORMAT` marker to raise `min_write_layout` above
   `SUPPORTED_LAYOUT` — simulating what a newer binary would write.
3. Asserts the open is refused with `StoreError::IncompatibleLayout`.
4. Restores the marker and reopens — asserts every row written before the
   refusal reads back, and that no migration reruns.

**What the pinned legs are, and who checks that.** `compat/releases.toml` pins v0.1.0
with `legs = ["upgrade"]`. The two tests that run that leg are
`a_v010_store_migrates_forward_and_keeps_reading_its_own_values` (open a store v0.1.0
wrote, and re-read every measurement its own server recorded while it held the files) and
`v010_compat_fre13_upgrade_contract` (assert the shape of the refusal). The test above is
a **substitute** for the `rollback` leg, not a run of it: it tampers the marker instead of
pointing an older binary at the candidate's store. `scripts/compat/test_leg_coverage.py`
holds that mapping as data — every leg of a pinned release is a runner, a substitute, or a
cited absence — and greps for the names it lists, so renaming a compat test without
updating the gate fails CI.

**No CI job performs a real old-binary rollback after writes.** The test above
expresses the layout-floor contract in-process; it does **not** run the v0.1.0
binary against a store that the current build has written to (no v0.1.0 binary
artifact is available in CI). Rollback-after-writes using a real older binary is
unsupported for 1.0.

### Rollback procedure for layout 2

To roll back the `weft-server` binary from v0.3.x to v0.2.1 (both layout 2):

1. Stop the newer server.
2. Replace the binary (or revert the container image tag).
3. If the audit log (`WEFT_AUDIT_FILE`) was at its default location under the store
   root, remove or rename it — the older binary does not know to ignore it and would
   fail to open the store if the file cannot be truncated. If `WEFT_AUDIT_FILE` is
   set explicitly, unset it or point it at a path the older binary will not check.
4. Start the older binary.

No restore of data is needed because the layout did not change.

---

## Version support window

| Layout | Releases that write it | Releases that can open it | Releases that refuse it |
|--------|------------------------|---------------------------|-------------------------|
| 1      | v0.1.0                 | v0.1.0 (no marker check)  | none (no marker)        |
| 2      | v0.2.0, v0.2.1, v0.3.x| v0.2.0+ (see below)       | v0.1.0 (writes anyway)  |

A WeftDB whose `SUPPORTED_LAYOUT` is `N` can **write** any store whose
`min_write_layout <= N`. The marker is written at `new_store()` time with both floors
at `SUPPORTED_LAYOUT`, and a legacy (layout 1) store gets `min_write_layout: 1` raised
to 2 after migration.

To make rollback possible across a minor version that does not change the layout,
**operators must keep the previous release's binary** (or the previous container
image tag) until they have verified the new release works. The repository publishes
pre-built binaries for every release; a v0.2.x binary is the safe rollback target
for a v0.3.x deployment that stays on layout 2.

---

## Release-blocking prerequisites

Two open decisions gate any formal 1.0 upgrade story:

- **Storage format decision** — whether the 1.0 bin can read data written by a 0.x
  build, and under what constraints. Operators must be told plainly: "1.0 can read
  a layout-2 store written by any 0.x binary (v0.2.0+), but a layout-1 store (v0.1.0)
  must be migrated to layout 2 before 1.0 opens it, and the migration is one-way."
- **Wire-format decision** — whether the 1.0 HTTP API accepts requests from a 0.x
  client and vice versa, which bears on rolling upgrades where a mix of 0.x and 1.0
  servers share a load balancer.

These decisions are tracked as release-blocking items (see
[`ROADMAP.md`](ROADMAP.md) and the 1.0 milestone). Until they are settled, the
upgrade path documented here for 0.x releases is provisional and does not constitute
a 1.0 compatibility promise.

---

## Unreleased — seal idempotency key change (#803)

The legacy seal adapter no longer derives the idempotency key from a CRC-32 of the payload.
Seal-backed typed-API writes without an explicit token now append (duplicate-timestamp writes
resolve newer-wins at read, matching rows-mode's no-unique-constraint behaviour).  Callers who
require byte-identical-retry replay must supply an explicit idempotency token: pass one to
`weftdb::legacy_seal::ingest_batch_with_gate` (a `Some("my-key")` token keeps a replay stable,
`None` mints a fresh token per call).  HTTP clients are unaffected by this change: the server
never enters the seal gate — it puts the `Idempotency-Key` header into `IngestBatch.key` and
the store's own idempotency ledger answers a replay with `replayed: true`, pinned by
`weft-server/tests/seal_idem_http_replay.rs` (#824).  The prior `legacy:{uuid}:{crc}:{n}`
ledger rows are inert historical entries; the adapter never looks them up.

## What is NOT guaranteed

- **A real rollback-after-writes CI test.** No CI job today writes data with the current
  build and reads it back with the previous release's binary. Rollback-after-writes is
  unsupported for 1.0: a store written by the current layout is not readable by older
  binaries. One of the three pieces that job needs now exists: `scripts/compat/releases.py
  fetch` downloads a pinned archive and refuses it unless its bytes hash to the digest in
  `compat/releases.toml`, and the `pinned release archives` job does that on every CI run.
  Two do not. The first is a scratch copy of the store to point the old process at, because
  a v0.1.0 binary classifies a layout-2 tree as layout 1 and writes into it — and that
  binary answers neither `--version` nor `--help`, it prints its configuration banner and
  starts listening, so a leg has to drive it through its environment and its HTTP endpoints.
  The second is a pinned release whose binaries can read the current layout: only v0.1.0 is
  pinned, and a newer pin block needs a committed fixture set of its own before
  `scripts/compat/releases.py verify` accepts it. Tracked as #762.
- **Cross-version binary compatibility for the server binary itself.** The v0.1.0
  `weft-server` binary cannot be swapped in for v0.2.x: its configuration variables,
  its command-line flags and its HTTP routes differ. The store-format compatibility
  described here covers only whether the *data on disk* is readable.
- **Read-only mode for older binaries.** A WeftDB that opens a store whose layout is
  newer than its `SUPPORTED_LAYOUT` refuses the store outright — it does not offer a
  "read what you can" fallback. The `min_read_layout` field is recorded for future
  binaries and not acted on (as documented in `store_format.rs`).
- **Fixture or golden data generated by builds other than v0.1.0.** Only v0.1.0 has
  a committed fixture set. A layout-2 store written by v0.2.x or v0.3.x is not
  captured as a fixture, so the forward-compatibility test (open a v0.2.x store with
  the current build) is not run.