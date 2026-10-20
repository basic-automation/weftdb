# `compat/fixtures/v0.1.0` — what the first public release wrote

Every byte under `store/`, `backup/` and `legacy/` was written by a **v0.1.0** binary built
from the `v0.1.0` tag, and is committed unchanged. `weftdb/tests/compat_v010.rs` opens them
with the build under test; that is the upgrade/rollback evidence for 1.0 plan slice X-FX.
`MANIFEST.sha256` holds one sha256 per file in the set — the three trees and every record
(109 files as committed),
`binaries.sha256` the digest of the two binaries that wrote them, and `provenance.txt` the
tag, the commit its annotated object peels to, and the toolchain. A v0.1.0 `weft-server`
has **no `--version` flag** — passing one starts a server — so the digest is how these trees
say what produced them.

Regenerate with:

```bash
scripts/compat/make-v010-fixtures.sh --bin-dir <dir with weft-server + gen_legacy_fixture built from v0.1.0>
```

## The three trees

- **`store/` — a Storage-v2 root as the tag leaves it.** Three aspects off one grid:
  `price` (declared `scaled-i64:4`, seconds, exact tolerance), `temp` and `flow` (f64,
  seconds). It arrived through three different ingest routes — CSV body for all three,
  JSON `points` for `price` (deliberately unsorted), ILP line protocol for `price` again
  (237 of its 244 rows: line protocol has no spelling for null, so the nulls entered by
  the other two routes). The aspects are left in the three shapes a reader can meet:
  `price` as three **overlapping** batches (3 segments, 725 rows, 14 nulls, 2 of them
  unsorted), `temp` as a sorted batch plus a reversed batch **reconciled in place** (2
  segments, 72 rows, 7 nulls), `flow` **squashed** to a single segment (80 rows).
  There is **no `store.json` marker at this version** — the marker postdates the tag — and
  no `segment_manifest.json`: the descriptors live in `segment_index.db`, which is why a
  reader has to classify the tree from what is in it rather than from what it says.
- **`backup/` — what `POST /api/v1/storage/backup` wrote** while that server owned the
  store, with the server's own report (`backup.json`) and the **restore-drill report**
  (`restore_drill.json`). What this version actually backs up is the control plane —
  `catalog.db`, `aspect_catalog.db`, `metadata.db`, `segment_index.db` — and **not**
  `segments/`. A v0.1.0 backup therefore cannot restore an ingested store by itself, which
  is worth knowing before anything promises a restore path across versions.
- **`legacy/` — a rows-mode database**, not a Storage-v2 root: `v010_legacy/metadata.db`
  plus one directory per subject/aspect, each holding its own Turso `measurements.db`,
  `pipeline.db`, `correlations.db` and friends. Written by the `gen_legacy_fixture` example
  compiled from the tag, through the v0.1.0 library's own write path.

## The records

- **`rows.csv`** — the shared grid (384 rows): 244 `price` rows at minute steps, 7 of them
  null, plus four rows far outside every series window — two at the epoch, one at
  `2147483648` (one past the 32-bit ceiling) and one at `4070900000` — because a
  second-scale store must survive those and a v0.1.0 read is the only claim about whether it
  did; 60 `temp` rows at two-minute steps, 6 null; 80 `flow` rows in two bursts an hour
  apart. One grid, three ingest routes, one `expected_reads.txt` describing what each must
  answer.
- **`expected_reads.txt`** — one `# kind aspect query` header above each body: three
  `range.csv` windows, a `value-range.csv`, an `at`, and a `downsample.csv` bucket sweep,
  every one answered by a v0.1.0 server. `aspect_stats.txt` holds the three `/{aspect}/stats`
  bodies the same way; `catalog.json`, `aspects.json` and `stats.json` are the store's own
  account of itself at the tag, kept so a migration that changes the shape has to say so.

## What is not byte-stable, and what is

Databases are not byte-stable (Turso stamps write times inside them), which is why the trees
are verified by **opening** them and `MANIFEST.sha256` only freezes what is in them today.
Two JSON records would otherwise move on every regeneration for reasons that are not data:
the backup label (`manual-<wall-clock>`, which the tag generates itself) and this run's temp
directory in the paths. The generator redacts both to `manual-<timestamp>` and `<work>`; no
other byte in any record is touched, and the on-disk backup directory keeps the real label.
Everything else — `expected_reads.txt`, `aspect_stats.txt`, `segment_listing.txt`,
`catalog.json`, `aspects.json`, `stats.json` — has been checked to reproduce byte-for-byte
from the tag's binaries.

## What the test proves

1. `open_report` names this root's layout **before** any write — a v0.1.0 root carries no
   marker, so it is classified from its shape — and the current layout **after**, with the
   store's values unchanged across that migration.
2. The migrated store **reopens stably**: the same format version, the same reads.
3. The recorded `range.csv` reads answer the same timestamps, values **and nulls**, over the
   full range and the windowed range — and the declared `scaled-i64:4` / seconds / exact
   schema on `price` survives, so the values come back at scale, not as a guess.
4. The shapes are the shapes: 6 segments for 3 aspects, overlapping, reconciled and squashed.

The honest gap: **`legacy/` is not readable by this build.** Rows-mode v0.1.0 stores their
measurements in Turso databases, and the current engine never opens Turso — the roadmap's
D-S4 entry says as much ("Nothing in the repo reads it back"). So `legacy/` is checked in as
the evidence of what FRE-14's migration has to consume, and its test asserts the *shape* of
what FRE-14 must read, not a read. Rows-mode has no null values either, so that tree is the
non-null subset of `rows.csv`; the counts differ from the store's by design.
