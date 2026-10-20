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

500 rows across three aspects, 540 KB total:

```text
<data_dir>/v010_legacy/
├── metadata.db (+ -wal, -log)      # subjects, aspects, the database row — turso, not sqlite
├── plant/price/                    # one directory per (subject, aspect)
│   ├── measurements.db (+ -wal, -log)
│   ├── dictionaries/
│   └── pipeline.db correlations.db patterns.db processed_*.db unprocessed_*.db
├── plant/temp/
└── plant-b/price/
```

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

## Still to do for X-FX

1. `scripts/compat/make-v010-fixtures.sh` — set up the worktree above, start a v0.1.0 server
   on `127.0.0.1` with `WEFT_SEGMENT_STORE_ROOT` in a temp dir, declare + ingest one aspect
   through all three formats, reconcile, read back, take a backup, then run the legacy
   generator; copy the three trees plus `MANIFEST.sha256` and `expected_reads.txt` into
   `compat/fixtures/v0.1.0/`.
2. A `compat/` crate whose tests open each fixture with the **current** build: the store
   fixture must report its format version through `describe_store()` before and after the
   migration and keep returning the same values; the legacy tree must become a v2 database
   that reads the same values; the backup must restore into an empty root.
3. A CI job for those tests (the fixture set is small on purpose — the large one comes at
   slice 17).

Until 1–3 land, nothing in this directory is exercised by CI; the generator has only been run
by hand, against a worktree at the tag, with the result above.
