# RPO/RTO (EVI-19)

What this page pins for the 1.0 release: the recovery-point objective and the
recovery-time objective against WeftDB's *measured* snapshot cadence — the
control-plane backup daemon (`WEFT_BACKUP_INTERVAL_SECS`) and the restore path
(offline CLI and the live restore drill).

## What this page pins

**RPO** (recovery-point objective) is the largest window of *acknowledged*
writes a crash can lose: writes the server returned `2xx` for, then lost, then
had to be re-sent. **RTO** (recovery-time objective) is the time from the start
of an outage to a restored store *serving* traffic again — including taking the
backup it restores from and running the restore itself. The honesty rule for
this page: every figure is one of three things. It is **measured** (a benchmark
in this repo produced it), **derived** (it follows from a setting's default by
arithmetic you can redo here), or **labelled unmeasured** with the command or
job that would produce it. No figure is stated as a plausible round number.

## The RPO formula

The durability design's RPO formula, corrected per plan decision D-S21
(`1.0-plan.md` row D-S21, which supersedes `crash-design.md:517`):

```
RPO = max(I, d_max) + d_max
```

In WeftDB's own terms:

- **`I` — the snapshot interval.** The backup daemon writes a verified snapshot
  on a fixed tick; `I` is that tick, `WEFT_BACKUP_INTERVAL_SECS`
  (`weft-server/src/settings.rs:102`). Its *default is disabled*: unset or `0`
  means "no scheduled backups"
  (`weft-server/src/startup/daemons.rs:213-218`), so there is no code default
  numeric value to quote. `I` is whatever the operator sets.
- **`d_max` — the window of committed-but-not-yet-snapshotted writes.** An
  ingest is acknowledged *after* its index COMMIT returns
  (`weftdb/src/types/segment_store/seal.rs:729`, the single
  `commit_index(&IndexTxn::new(ops))` under the per-aspect commit lock), and the
  store's durability class is `full` off Windows — the seal fsyncs its file and
  its directory entry, and Turso FULL-syncs every returned COMMIT
  (`weftdb/src/types/segment_store.rs:1572-1576`,
  `weftdb/src/types/segment_store/swap_tests.rs:187`). A snapshot, by contrast,
  is a *point-in-time* image: Turso's `VACUUM INTO` captures a
  transactionally-consistent copy as of when the vacuum begins
  (`weftdb/src/types/backup.rs:300`). So writes that COMMIT *while a snapshot
  pass is running* — after the vacuum's image was taken but before the next
  snapshot begins — are acknowledged, durable, and **absent from that
  snapshot**. The width of that window is `d_max`. It is not a setting: it is
  the snapshot pass's own duration (vacuum, verify, fsync, manifest write,
  rename, parent fsync — `weftdb/src/types/segment_store/maint.rs:249-251`),
  which the daemon runs between ticks.

**Worked example.** The daemon is disabled by default, so an operator must pick
`I`. Take `I = 60 s` (an *assumed configured* value, not a code default). If a
snapshot pass takes `d_max = 4 s` — the daemon's ticks on this project's btrfs
volume landed 3.0–4.5 s apart at a 2 s configured interval (the measured figure
in [reference.md](reference.md)) — then:

```
RPO = max(60, 4) + 4 = 64 s
```

The `max` term is why a snapshot slower than its interval hurts twice: once as
the interval stretches to the pass length, once again as the pass's own window
is added. `d_max` itself is **unmeasured** for any given deployment — see
[Unmeasured](#unmeasured) for how to measure it.

## Snapshot cadence as configured

Straight from the settings registry (`weft-server/src/settings.rs`), which is
the single source for names, tiers, defaults and readers:

| Variable | Tier | Default | What it does |
|----------|------|---------|--------------|
| `WEFT_BACKUP_DIR` | Stable | `<store root>/backups` | Where control-plane backups are written — shared by the daemon and `POST /api/v1/storage/backup` (`weft-server/src/settings.rs:101`). |
| `WEFT_BACKUP_INTERVAL_SECS` | Tuning | unset or `0`: no scheduled backups | `I`. How often the daemon writes a verified `VACUUM INTO` snapshot of the four control-plane databases; `0` disables it (`weft-server/src/settings.rs:102`). |
| `WEFT_BACKUP_KEEP` | Tuning | unset: retain every generated snapshot (packaged deployments set 3) | How many **daemon-generated** snapshots to keep; a hand-taken `?label=` snapshot is never pruned (`weft-server/src/settings.rs:103`). |

The daemon reads `WEFT_BACKUP_INTERVAL_SECS` and returns early when it is `0`
(`weft-server/src/startup/daemons.rs:213-218`), otherwise it spawns on that
interval (`weft-server/src/startup/daemons.rs:226-236`). Each tick writes one
snapshot through `backup_tick`
(`weft-server/src/backup_daemon.rs:307`), and the first snapshot fires **one
interval after start** — the immediate tick is consumed so start-up is not
stampeded (`weft-server/src/backup_daemon.rs:345-348`). A snapshot contains
exactly the four control-plane databases and a `MANIFEST.json` — the `.weftseg`
measurement frames are **not** in a control-plane snapshot
(`weftdb/src/types/backup.rs:383`, storage boundary / hard-constraint #3).

## RTO, and what is actually measured today

Two paths put a backup back, and neither returns a duration.

- **Offline restore CLI (PAC-13).** `weft-server restore <BACKUP> --into
  <NEW_ROOT>` (`weft-server/src/restore.rs:62`, `:222`) is the path a real
  recovery takes when the store is gone: it reads the backup's `MANIFEST.json`,
  picks whole-store vs control-plane (`weft-server/src/restore.rs:344`), refuses
  a destination that exists, and publishes a fresh root. Its `Report`
  (`weft-server/src/restore.rs:184`) carries `mode`, `backup`, `root`, `intent`,
  `frames`, `rows`, `bytes`, `files[]`, `adoption` — **no elapsed-time field**.
  The only timestamp it prints is `restore_started_ms`, an absolute start time
  from the restore-intent marker
  (`weft-server/src/restore.rs:257`,
  `weftdb/src/types/backup.rs:940`), not a duration.
- **Restore drill route.** `POST /api/v1/storage/restore/drill?label=`
  (`weft-server/src/backup_routes.rs:79`) rehearses a restore on a *running*
  server into a throwaway directory, verifies every restored database opens and
  every row reads, then deletes the copy. Its `RestoreDrillResponse`
  (`weft-server/src/backup_routes.rs:36`) returns `label`, `databases`,
  `total_rows` (`:43`), `total_bytes` (`:45`), `restorable` (`:48`) and
  `cleanup_error` (`:53`) — again **no duration**. The online backup response
  (`BackupResponse`, `weft-server/src/backup_routes.rs:175`) likewise returns
  `dir`, `databases`, `total_rows` (`:182`), `total_bytes` (`:184`) and `verify`,
  not a timing.

So **no measured restore or backup duration exists in the code today**. The
`total_bytes` the drill and the backup endpoint return are the size of the
restored/snapshotted data, and the daemon's only counters are
`weft_backup_snapshots_total` and `weft_backup_bytes_written_total`
(`weft-server/src/metrics.rs:389`, `:422`) — bytes, never seconds. Where a
duration is quoted on this page it is the figure from
[reference.md](reference.md) produced by a *run* (the `tmpfs` backup-and-verify
times, the btrfs tick spacing), never a value this document computes.

Measured RTO inputs — how long a backup takes, how long a restore takes, how
long recovery takes end to end — are the deliverable of two slices that are
still open, so this page does not invent a duration:

- **[EVI-25](https://github.com/basic-automation/weftdb/issues/295)** — "EVI-25
  Backup, restore and recovery cost measurement (RTO inputs and backup
  duration)".
- **[SOA-11](https://github.com/basic-automation/weftdb/issues/308)** — "SOA-11
  Soak backup lane, online prefix check, final restore verification, RPO/RTO".

Until those land, **RTO is `unmeasured`**.

## Unmeasured

Every figure below is not sourced from code or a setting default on this page.

- `d_max` (the snapshot pass duration that sets the RPO's second term) —
  unmeasured — take a `WEFT_BACKUP_INTERVAL_SECS` series on the target volume
  and time one `backup_tick` from its `backup.tick` span start to the
  rename/fsync (`weft-server/src/backup_daemon.rs:307`), or run
  `weftdb/benches/backup_cost.rs` against the target volume with
  `WEFT_BENCH_BACKUP_DIR` set.
- **Backup duration** on the operator's storage —
  unmeasured — EVI-25 (issue #295) measures it; meanwhile the tmpfs and btrfs
  figures in reference.md are the only dated numbers.
- **Restore duration** (offline CLI and drill) —
  unmeasured — EVI-25 (issue #295) times `weft-server restore` and the drill
  route; neither currently returns a duration field.
- **End-to-end RTO** for a deployment —
  unmeasured — SOA-11 (issue #308) times the soak backup lane and the final
  restore verification.
- **`I` for a deployment** — no code default exists;
  unmeasured — read it back from the startup line the daemon prints
  (`weft-server/src/startup/daemons.rs:234`), or from the operator's config. The
  value this page's worked example uses (60 s) is assumed, not configured.

## Reading order

- [docs/reference.md](reference.md) — the backup & restore section and the
  Configuration table this page summarises.
- [Issue #215](https://github.com/basic-automation/weftdb/issues/215) —
  "Document RPO/RTO against the measured snapshot cadence" (this page closes it).
- [Issue #308](https://github.com/basic-automation/weftdb/issues/308) —
  "SOA-11 Soak backup lane, online prefix check, final restore verification,
  RPO/RTO" (the slice that measures the RTO inputs).
- [Issue #295](https://github.com/basic-automation/weftdb/issues/295) —
  "EVI-25 Backup, restore and recovery cost measurement (RTO inputs and backup
  duration)".
