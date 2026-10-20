# The v0.1.0 side of the compatibility fixtures

1.0 plan slice **X-FX** requires one fixture set under `compat/fixtures/v0.1.0/` — a
Storage-v2 database, a backup archive, and a legacy rows-mode database, **each written by a
v0.1.0 binary**, with the generating command recorded. Nothing in this directory is compiled
by the workspace: the generators here are written against the **v0.1.0 API** and are compiled
from a worktree checked out at the `v0.1.0` tag, then their output is committed here. Keeping
them out of the crate's own `examples/` tree is deliberate — a later tree would compile them
against a later API and they would stop describing what the release wrote.

## What is here

- `weftdb/examples/gen_legacy_fixture.rs` — the legacy rows-mode generator. Copy it into a
  v0.1.0 worktree at `weftdb/examples/` and build it there. It reads
  `subject,aspect,timestamp,value` rows (no header) and writes them through the v0.1.0
  library's own write path (`observe_subject` → `track_aspect` → `capture_measurement`), so
  whatever layout v0.1.0 produced is what lands on disk.

## How to build it (verified against the tag)

```bash
git worktree add -f /tmp/weft-v010 v0.1.0
cp compat/v010/weftdb/examples/gen_legacy_fixture.rs /tmp/weft-v010/weftdb/examples/
(cd /tmp/weft-v010 && cargo build --locked --release -p weftdb --example gen_legacy_fixture -p weft-server)
```

`cargo 1.100.0-nightly` builds that tag with no source change (checked: `weft-server`,
`weftdb` and the example all compile clean from `v0.1.0`).

## What that generator leaves on disk (checked by running it)

The committed fixture runs it over `compat/fixtures/v0.1.0/rows.csv` — 384 rows for
`plant/{price,temp,flow}`, of which 13 are null — and the tree is ~490 KB:

```text
<data_dir>/v010_legacy/
├── metadata.db (+ -wal, -log)      # subjects, aspects, the database row — turso, not sqlite
├── plant/price/                    # one directory per (subject, aspect)
│   ├── measurements.db (+ -wal, -log)
│   ├── dictionaries/
│   └── pipeline.db correlations.db patterns.db processed_*.db unprocessed_*.db
├── plant/temp/
└── plant/flow/
```

Rows-mode has no null value, so the generator **skips** null grid rows rather than writing a
sentinel; `legacy/` is therefore the non-null subset of the grid the Storage-v2 tree holds.
It says so on stderr (`skipped N row with no value for rows-mode`) so the count is on the
record rather than in a comment.

`metadata.db` is a **turso** file, not SQLite — `python3 -c "import sqlite3; …"` refuses it
("file is not a database"), so the fixture can only be read by the crate that wrote it. That
is part of why it has to be checked in rather than regenerated in CI.

## The v0.1.0 binary's environment names — which are *not* today's

A v0.1.0 server reads a different set of variables from a current one. These are the names as
of the tag (from `weft-server/src/main.rs` and `weftdb/src/types/database/config.rs`):

| what | v0.1.0 name | today |
|---|---|---|
| rows-mode data directory | `TEST_DATA_DIR`, else `WEFT_DATA_DIR`, else the built-in default | the configuration registry |
| Storage-v2 root | `WEFT_SEGMENT_STORE_ROOT` | `WEFT_STORE_ROOT` |
| backups | `WEFT_BACKUP_DIR` (else `<store root>/backups`) | same name |
| listen address | `WEFT_SERVER_ADDR` | same name |

## The v0.1.0 HTTP surface the fixture script drives

Verified present at the tag (`weft-server/src/lib.rs`'s router):

- `POST /api/v1/storage/aspects` — declare (body: `database`, `subject`, `aspect`,
  `resolution`, `codec`, `scale`, optional `time_unit`)
- `POST /api/v1/storage/{aspect}/csv` — ingest `timestamp,value` lines (an optional
  `timestamp,value` header row is skipped; an empty value is a null)
- `POST /api/v1/storage/{aspect}/ilp` and `/points` — the same rows in ILP and JSON
- `POST /api/v1/storage/reconcile` and `/api/v1/storage/{aspect}/reconcile` — flush and
  re-sort the segments a store holds
- `POST /api/v1/storage/backup` — the archive the `backup/` fixture is
- `GET /api/v1/storage/{aspect}/range.csv` (and `.parquet`) — the reads recorded into
  `expected_reads.txt` while the v0.1.0 server is still the one answering them
- `GET /api/v1/storage/catalog`, `/api/v1/storage/aspects`, `/api/v1/storage/stats`

## What X-FX landed with

1. `scripts/compat/make-v010-fixtures.sh` builds nothing; it takes a `--bin-dir` holding
   `weft-server` and `examples/gen_legacy_fixture` compiled from the tag, starts a v0.1.0
   server on `127.0.0.1` with `WEFT_SEGMENT_STORE_ROOT` in a temp dir, declares three
   aspects, ingests one grid through CSV / JSON `points` / ILP, leaves the aspects in the
   three shapes a reader meets (overlapping batches, a batch reconciled in place, a squashed
   aspect), records every read it can make, takes a backup, runs the restore drill, then runs
   this generator. Its output is `compat/fixtures/v0.1.0/`, `MANIFEST.sha256` included.
2. `weftdb/tests/compat_v010.rs` opens each tree with **this** build. Not a separate
   `compat/` crate: the thing under test is `SegmentStore`'s own open path, and a crate that
   depends on `weftdb` would test the same code through an extra layer while sitting outside
   the build matrix that already runs `weftdb`'s integration tests on the release compiler,
   the MSRV and Windows.
3. CI needs no new job: the build matrix runs `weftdb`'s tests, and the `release pins` job
   runs `scripts/compat/releases.py verify`, which re-hashes every file in the fixture set the
   pin names — so a fixture byte changed by accident fails there, not in a test three months
   later.

**What the fixture settled, and what it did not.** A v0.1.0 Storage-v2 root carries no
`store.json` and no `segment_manifest.json`: the descriptors live in `segment_index.db`. This
build classifies such a root as layout 1, migrates it forward, and reads back the same
timestamps, values and nulls the v0.1.0 server recorded — then reopens without migrating
again. A v0.1.0 backup covers the four control-plane databases and **none** of `segments/`, so
it cannot restore an ingested store; that is a finding about the release, not a wish.
A v0.1.0 **rows-mode** tree is not readable by this build at all — rows-mode measurements sit
in Turso databases and the current engine never opens Turso (roadmap D-S4: "Nothing in the
repo reads it back"). `legacy/` is committed as the input FRE-14 has to work from, and its
test asserts the shape of what that migration must consume rather than pretending to read it.
