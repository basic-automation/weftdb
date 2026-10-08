# WeftDB 1.0: crash consistency and seal-routed ingest (final design)

Scope: ROADMAP 7.2, "WAL & crash consistency" (ROADMAP.md:698-699), and Immediate next action #1, "route measurement bulk ingest through the .weftseg seal" (ROADMAP.md:1132-1138).

Hard constraint: libSQL/Turso stays the control plane only. It holds descriptors, allocators, the frame journal, the idempotency ledger, the rollup and change rows. Measurement bytes live only in `.weftseg` frames. No sled, no sqlx.

---

## 0. Decision summary and lineage

**Base design: "Atomic Publish"** (highest score, 19). There is no separate WAL. Five rules carry the design:
1. The write-once, fsynced `.weftseg` frame is the durability record.
2. Exactly one `segment_index.db` transaction is the commit point for each state change.
3. A file is published only once it is complete and durable.
4. Superseded files are unlinked only after the commit that supersedes them is durable.
5. Startup reconciles the index against the directory before the server serves.

**Grafted from the other two designs and from the judges' best-idea lists:**
- **Never-reused frame names (from WOF/SCM), made deterministic.** A frame name carries a per-aspect, persisted, monotonic *generation*, not a random nonce. The reaper and recovery unlink only by exact name. They never unlink a name that a live row references; this guard comes from SNW.
- **Precedence fix that none of the three designs had: the precedence id is assigned inside the commit transaction, under a per-aspect commit lock.** Per aspect, ids therefore grow in commit order ("last committed wins").
  - Maintenance outputs inherit member ids: the target takes the min member id and the overlap-split suffix takes the max member id.
  - So no output can ever outrank a write committed after the maintenance snapshot. That includes an in-flight seal that allocated earlier and committed later, which breaks both "fresh suffix id" (Atomic Publish) and "max member id with allocate-before-write ids" (WOF/SCM).
- **Fail-stop on an ambiguous COMMIT**, implemented as a write poison (from Atomic Publish). Reads stay up; `WEFT_ON_AMBIGUOUS_COMMIT=exit` exits instead. Turso's MVCC log fsync error propagates with `?` (turso_core-0.8.1 mvcc/database/mod.rs:3545). By contrast, Turso's own WAL pager path *panics* on an fsync error by default (storage/pager.rs:4723-4740, data_sync_retry off at database.rs:2593), which is the precedent for fail-stop.
- **The rollup moves into `segment_index.db`** and is folded inside the same transaction (Atomic Publish, judge 2). Swaps fold a delta, not a full rescan.
- **Per-aspect locks plus Turso's built-in MVCC group commit**, with no store-wide publisher. Group commit is on by default (turso_core-0.8.1 mvcc/database/group_commit.rs:82, used at mvcc/database/mod.rs:3401-3403). A `DirSyncer` coalesces directory fsyncs (WOF/SCM).
- **Reader reclaim pins plus a backup gc-hold**, not a timed grace (WOF/SCM).
- **Backups without an ingest barrier.** `segment_index.db` is snapshotted first and is the authority, then frames are hard-linked under the gc-hold (WOF/SCM).
- **Relaxed mode bounded by a `synced_epoch` watermark** stored in the FULL-synced DB (Atomic Publish). It does not infer the previous mode from a lock file.
- **Restore mode** (a `RESTORED` marker) that adopts CRC-valid unindexed frames, with descriptors rebuilt from the frame (Atomic Publish, SNW).
- **Ledger rows are invalidated** when the data under them is quarantined (all judges).
- **Encode on `spawn_blocking`**, MVCC and `synchronous=FULL` asserted at open, and the ineffective NORMAL pragmas removed (WOF/SCM).
- **Legacy routing is opt-in per aspect** (`storage_mode`), and the per-timestamp queue is replaced by change rows (Atomic Publish).
- **SNW's WAL front end** for small writes is deferred past 1.0 (judge 2).

**Scope and milestones.** 21 slices, about 137 h, each 2.5-8 h.
- **M1** (S1-S12 plus S15, about 88 h): the Storage-v2 strict promise is true, including under maintenance and power loss.
- **M2** (S13-S14): idempotent atomic HTTP ingest.
- **M3** (S16): whole-store backup and restore.
- **M4** (S18-S20): legacy ingest on the seal, which delivers Immediate #1.
- **S17** (relaxed mode) is optional. **S21** is CI and the benchmark.

---

## 1. Problem

### 1.1 What a 2xx means today (verified)

**HTTP seal:** JSON, CSV, ILP and Parquet go through `persist`/`persist_paged` (weftdb/src/types/segment_store.rs:588-632). In order:
- The id comes from an unreserved `SELECT MAX(id)+1` (weftdb/src/types/segment_index.rs:303-312).
- The frame is written with a truncating `tokio::fs::write` straight to the final name `{aspect}-{id}.weftseg` (segment_store.rs:596-597, 619-620, 635-637). There is no fsync of the file or the directory anywhere in the workspace.
- `INSERT OR REPLACE` writes the index row and commits it with an fsync (segment_index.rs:142-161). The connection defaults to `SyncMode::Full` (turso_core-0.8.1 database.rs:2591) and fsyncs the logical log at mvcc/database/mod.rs:3537-3551.
- The rollup is updated by a get-then-put in a second DB (weftdb/src/types/metadata.rs:269-273).
- The sidecar is written before the ack (segment_store.rs:603-608).

So a 201 survives a process crash. It does **not** survive power loss, a concurrent seal on the same id, a later in-place maintenance rewrite (segment_store.rs:983/988, 1015/1020), or a restore. It is not idempotent.

**Legacy bulk ingest** (weftdb/src/types/database/inputs.rs:214-346):
- Each chunk commits on its own.
- The queue, dirty marks and timestamps follow the chunks (inputs.rs:330-344).
- Acknowledged `metadata.db` commits are deleted at the next cold open (weftdb/src/types/database/mod.rs:80-86).

**Control-plane backups** contain no frames (segment_store.rs:419-421). They are four snapshots taken at four different instants (segment_store.rs:455-458), with no manifest and no directory fsync.

### 1.2 Verified windows (43). Every one is mapped in section 12.

| Group | Windows |
|---|---|
| Seal (4) | seal-orphan-frame-before-index-commit; seal-index-committed-rollup-not-folded; seal-committed-unacknowledged; seal-acked-frame-not-durable |
| Sidecar (4) | sidecar-torn-write-process-crash; sidecar-powerloss-zero-tail-no-crc; seal-or-rewrite-crash-before-sidecar; orphan-sidecar-matches-reused-id |
| Maintenance (12) | inplace-reconcile-torn; inplace-reseal-torn; powerloss-inplace-reconcile; powerloss-merge-members-deleted-target-not-durable; powerloss-split-halves-unordered; rewrite-written-before-index-swap; merge-paged-target-format-wedge; split-duplicate-window; split-suffix-orphan; merge-target-committed-members-not-deleted; member-index-deleted-file-not-removed; maintenance-rollup-not-rebuilt |
| Races (4) | race-next-id-collision; race-max-id-reused-between-delete-and-unlink; race-reconcile-resurrects-merged-member; race-metadata-rollup-lost-update |
| Legacy (8) | legacy-open-deletes-mvcc-logical-log; legacy-batch-partial-prefix; legacy-measurements-committed-not-queued; legacy-committed-before-ack; legacy-capture-measurement-tail; legacy-queue-consumer-batch-before-dequeue; legacy-database-new-half-created; legacy-power-loss-inflight-chunk |
| Registry (1) | open-scoped-register-database-then-subject |
| Backup/restore (10) | backup-vacuum-into-partial-dest; backup-partial-dir-counts-toward-retention; backup-dirent-not-durable; backup-cross-db-skew-vs-seal; backup-captures-mid-maintenance; restore-beside-newer-frames-id-reuse; restore-into-new-root-reads-old-paths; prune-partial-remove; drill-dir-leak; verify-sidecar-litter |

**Severity.**
- Acknowledged data loss (7): seal-acked-frame-not-durable, inplace-reseal-torn, race-next-id-collision, race-max-id-reused-between-delete-and-unlink, legacy-open-deletes-mvcc-logical-log, restore-beside-newer-frames-id-reuse, legacy-power-loss-inflight-chunk.
- Corruption of committed data (6): seal-orphan-frame-before-index-commit, inplace-reconcile-torn, powerloss-inplace-reconcile, powerloss-merge-members-deleted-target-not-durable, powerloss-split-halves-unordered, race-reconcile-resurrects-merged-member.
- Everything else is recoverable inconsistency, a leak, or benign.

**Refuted, not designed for:** restore-partial-control-plane and restore-unsynced-copy-power-loss. Their only production caller is the drill, which restores into a throwaway directory (weft-server/src/manage.rs:548-551). S5 still fsyncs restored files because it is cheap.

### 1.3 Turso facts this design relies on (verified in turso_core-0.8.1 source)

- **Synchronous mode.** Every new connection defaults to FULL (database.rs:2591). `PRAGMA synchronous` is readable (translate/pragma.rs:1615) and is set per connection (translate/pragma.rs:656-677).
- **Ineffective NORMAL pragma.** WeftDB's NORMAL pragma lands on throwaway connections (weftdb/src/types/database/connection.rs:85-96, database/mod.rs:109), so FULL is what applies today. S1 deletes those lines so nobody "fixes" them into effect.
- **Log fsync and errors.** COMMIT under FULL fsyncs the MVCC logical log. A sync error is returned with `?` (mvcc/database/mod.rs:3545). No poison logic exists on that path.
- **Group commit** is enabled by default (mvcc/database/group_commit.rs:82) and used for non-exclusive commits (mvcc/database/mod.rs:3401-3403).
- **The log is truncated in place on checkpoint** (mvcc/persistent_storage/logical_log.rs:1032-1046, 1054-1093). It is not unlinked and recreated. So once the `-log` directory entry has been fsynced (S3), it stays durable. This refutes the "Turso recreates -log" risk raised against Atomic Publish.
- **The switch to MVCC does not fsync the DB file.** `PRAGMA journal_mode=experimental_mvcc` writes page 1 with the MVCC header (vdbe/execute.rs:19149-19160, `OpJournalModeSubState::WritePage`) and never syncs it. Later COMMITs fsync only the `-log`; the DB file is next fsynced by a checkpoint that backfills frames (mvcc/database/checkpoint_state_machine.rs:2979-3011), which comes at the 4 MB log threshold (logical_log.rs:270) or when the last connection closes. The open's probe reads the header back from cache, so it proves the mode in memory, not on disk. A power cut on a brand-new store before that first checkpoint can leave a header that still reads WAL beside a `-log` of acknowledged commits, and Turso then refuses to open the file ("MVCC logical log file exists ... header indicates WAL mode", database.rs:2091-2096). The obvious fix, `File::open(db)?.sync_all()` from WeftDB, is wrong: Turso's lock is a process-associated `F_SETLK` (rustix `fcntl_lock`), which POSIX releases when *any* descriptor the process has on the file closes, so the fsync would silently unlock the DB file for every other process. **Closed by S6** (which owns the open's schema migration). Measured with a recording Turso I/O backend (`weftdb/src/types/durable/turso_probe.rs`): on a database that never ran MVCC, the switch's MVCC metadata bootstrap backfills a new internal table through a checkpoint and does fsync the DB file; on one that already carries that metadata (an MVCC database switched back to WAL) nothing syncs page 1, and a TRUNCATE checkpoint on its own backfills and syncs nothing either. So every open commits a no-op transaction (`user_version` rewritten with its own value) and then runs a TRUNCATE checkpoint, which makes Turso rewrite page 1 and fsync the DB file, header included, before the open returns (`control_plane::enable_mvcc_full`). Every open, not only the one that switched: whether an open switched can only be judged from the header Turso reads back, which may come from the OS page cache. An open that crashed or failed between its switch and that sync (the checkpoint's fsync returning EIO, say) leaves an unsynced MVCC header that the next open reads as MVCC, and a next open that skipped the sync on that evidence would acknowledge commits that fsync only the `-log`. The price is one commit and one checkpoint per database per open. An upstream Turso fix that syncs page 1 in `op_journal_mode` would make that step unnecessary.
- **DB files are fcntl-locked exclusively on open** (io/unix.rs:67-71, 278-299). Two processes already cannot open the same DB file, so a root LOCK file adds a clear error message and covers `segments/`. It does not take away a working multi-process mode.
- **PASSIVE checkpoints are rejected under MVCC** (translate/pragma.rs:943-948). WeftDB swallows that error (connection.rs:146-157).

---

## 2. Durability promise

The mode is set store-wide by `WEFT_DURABILITY`. The default is `strict`. It is enforced inside `SegmentStore`, so HTTP and the legacy adapter behave the same.

**Strict (default).** A 201 from any ingest route, an Ok from `SegmentStore::ingest`/`seal*`, or an Ok from legacy `batch_capture_measurements` on a seal-backed aspect means:
1. **(a)** Every row is in one or more new `.weftseg` frames. Each frame was created with `create_new` under a never-used name, written, and `sync_all`'d, and its `segments/` directory entry was fsynced, all before
2. **(b)** one FULL-synced `segment_index.db` transaction published every frame of the call together, along with the rollup fold, the id/gen high-water marks and the idempotency-ledger row.
3. **(c)** The batch survives process crash, SIGKILL, SIGTERM, OS crash and power loss, on a device that honours flush. On Apple targets, std's `sync_all` uses F_FULLFSYNC.
4. **(d)** It is all-or-nothing for readers, backups and recovered stores.
5. **(e)** No later maintenance can lose it. Frames are never rewritten, and a superseded frame is unlinked only after a durable commit references its rows in new, fsynced frames.
6. **(f)** Newer-wins is "last committed wins" per aspect, before and after any maintenance.
7. **(g)** With an `Idempotency-Key`, the batch is applied at most once per (aspect, key) within `WEFT_IDEMPOTENCY_TTL_SECS` (default 7 days). A retry returns the original receipt as 200 with `Idempotency-Replayed: true`.

**Non-2xx responses:**
- **400:** definitely not stored. Encoding and tolerance checks are unchanged.
- **409:** the key was reused with a different payload.
- **413:** the body is over the limit.
- **500 `committed:false`:** a definite failure before COMMIT. The call's own frames are unlinked.
- **503 `outcome_unknown`:** COMMIT itself returned an error. The batch may be committed, so retry with the same key. The store is now write-poisoned until restart.
- **503 `poisoned`:** writes are refused until restart.
- **503 + Retry-After:** backpressure.

**Relaxed (opt-in, slice S17).** Same protocol, minus the per-frame and directory fsync on the *seal* path; the COMMIT stays FULL. A 2xx then guarantees (b), (d), (e), (f) and (g), and survival of a process crash. On power loss, batches committed after the last `synced_epoch` of their aspect may be lost. They are detected by the CRC check in recovery, quarantined, and their ledger rows invalidated. Reads never error, and the loss count appears on `/ready`. Maintenance and backup I/O is always synced.

**Legacy rows-mode aspects** (the default until the user decides otherwise; see the decisions):
- Today's per-chunk FULL commits stay, and they are non-atomic across chunks (inputs.rs:242-305).
- S1 stops the restart-time deletion of acknowledged `metadata.db` commits.
- S18 adds a write-ahead enqueue and batch dedupe.
- A partial prefix after a mid-call failure, and duplicates on a retry, remain documented residuals.

**Not promised in any mode:**
- durability of a batch that was never acknowledged;
- exactly-once without a key (the guarantee there is at-least-once; an exact duplicate is an overlap that `reconcile_overlaps` removes);
- invisibility after a 503 `outcome_unknown`;
- bit-rot repair (it is detected and quarantined);
- power-loss safety on Windows, where std cannot fsync a directory, so strict mode is process-crash-only there and `/ready` reports the durability class;
- NFS;
- PITR.

---

## 3. Invariants (checked after every recovery in tests)

- **I1.** Every strict 2xx batch is readable row-for-row, exactly once.
- **I2.** No range, point, points, downsample or value-range read errors after recovery.
- **I3.** No batch is ever partially visible. An unacknowledged batch is visible only if its COMMIT happened (the ambiguous case), and a keyed retry then replays it.
- **I4.** `fsck` is clean:
  - every live row's frame exists with `len == byte_len` and, when set, trailer == `frame_crc`;
  - no unreferenced frame exists outside `quarantine/`;
  - `frame_journal` is empty after recovery;
  - the rollup equals `AspectMetadata::from_index` (metadata.rs:67-72);
  - every sidecar matches its frame.
- **I5.** Maintenance never changes logical content: the newest value per timestamp is identical before and after.
- **I6.** Per aspect, ids and gens are never reissued, and frame names are never reused.
- **I7.** At a shared timestamp the later-committed write wins, before and after maintenance.
- **I8.** Recovery is idempotent: a second run is a no-op.
- **I9 (relaxed).** After power loss, only rows with `commit_epoch > synced_epoch` may be quarantined.
- **I10.** A replayed receipt never refers to quarantined data.

---

## 4. On-disk changes (all additive; the `.weftseg` format is unchanged)

**Frame format.** Single-block stays v5 (weft-physical-type/src/segment.rs:65), paged stays v6 (weft-physical-type/src/page.rs:54), and the CRC-32 trailer is unchanged (weft-physical-type/src/weftseg.rs:2164, read-side check 2316-2325). The frame stores no id or aspect: the header holds only stats and columns (weftseg.rs:2153-2166). So ids can be assigned at commit time, and adoption can rebuild descriptors with `SegmentDescriptor::of_segment` / `of_paged_segment` (weft-physical-type/src/catalog.rs:95, 107).

**Frame names under `segments/`:**
- **Legacy, generation 0:** `{aspect}-{id}.weftseg`, as today (segment_store.rs:635-637). These files stay valid and are never rewritten.
- **New:** `{enc(aspect)}~g{gen}~p{prec}.weftseg`.
  - `enc` keeps the bytes `[a-z0-9_]` literal and writes every other byte, including uppercase letters, `-`, `.`, `/`, `~` and `%`, as `%XX` in uppercase hex. The result contains no `/`, `.`, `-` or `~`, so there is no traversal (today the raw name is joined at segment_store.rs:636). Lowercase literals and uppercase hex escapes can never coincide, so the mapping stays injective even under case folding, which removes the Windows/macOS case-collision risk without rejecting any existing aspect name. Every non-ASCII byte is escaped as well, so an encoded name is plain ASCII and a normalising filesystem (HFS+) cannot merge the NFC and NFD spellings of one name into one file either. Both properties are requirements: legacy `{aspect}-{id}` names still collide in both ways (the aspect-name validator rejects names that leave `segments/`, name a device or display deceptively, not names that differ only in case or normalisation), and S10 is what removes the collision for new frames.
  - `enc` can triple a name's length, and the aspect-name validator already accepts names up to 160 bytes (`MAX_ASPECT_NAME_BYTES`). Such a name with uppercase letters, `-`, `.` or non-ASCII bytes encodes to as much as 480 bytes, and the `~g{gen}~p{prec}.weftseg` suffix adds more, well past the common 255-byte file-name limit. S10 must store every name the validator accepts, for example by falling back to a truncated encoding plus a hash of the full name when the encoded file name would be too long (the catalog keeps the real name). Otherwise the 160-byte cap has to be lowered before S10 ships, which would strand aspects already declared under longer names.
  - `gen` comes from a per-aspect counter that is never reused. `prec` is the adoption-order key: `gen` for seals, and the max member `prec` for maintenance outputs (legacy members count as 0).
  - Legacy names always contain `-{digits}.weftseg`; new names never contain `-`. The two forms are therefore disjoint.
- **Sidecars:** `{frame stem}.weftpart`. For legacy frames this is exactly today's `{aspect}-{id}.weftpart` (segment_store.rs:641-643).
- `segments/quarantine/` holds frames moved aside by recovery. They are purged after `WEFT_QUARANTINE_TTL_DAYS` (default 7).
- `.tmp-*` files are used only for sidecar replacement.

**Root files:**
- `LOCK`, held with std `File::try_lock` (workspace rust-version 1.95, Cargo.toml:26). The holder records its pid and session in `LOCK.holder` beside it, because a Windows lock is mandatory and would stop a second opener from reading a record kept in `LOCK` itself.
- `RESTORED`, a transient marker written by an in-place restore.

**`segment_index.db`.** The DDL runs outside BEGIN CONCURRENT, because DDL inside it fails (ROADMAP.md:1316-1318). Duplicate-column errors are ignored, following the existing pattern at metadata.rs:180.
- New columns on `segment_index`:
  - `gen INTEGER NOT NULL DEFAULT 0`
  - `prec INTEGER`
  - `frame_crc INTEGER`: NULL on legacy rows, meaning not yet bound; the backup/scrub path fills it in.
  - `commit_epoch INTEGER`
- `path` keeps being written as the root-joined path, so a downgraded binary can still open new frames. Readers resolve it against the current root (S6).
- New tables:
  - `aspect_seq(aspect PK, next_id, next_gen, epoch, synced_epoch)`: one row per aspect, so commits on different aspects never touch the same MVCC row.
  - `frame_journal(name PK, aspect, state 'pending'|'retired', retire_epoch, created_ms)`
  - `ingest_ledger(aspect, key, fingerprint, row_count, min_ts, max_ts, id_lo, id_hi, receipt_json, commit_epoch, created_ms, PK(aspect, key))`
  - `segment_quarantine(aspect, id, gen, name, reason, descriptor_json, quarantined_ms, PK(aspect, id, gen))`
  - `store_meta(key PK, value)`: `layout_version=2`, `store_uuid`, `rollup_migrated`, `clean_shutdown`.
  - `aspect_metadata`: the rollup, with the same columns as metadata.rs:161-173 (S11).
  - `segment_changes(aspect, epoch, min_ts, max_ts, row_count, PK(aspect, epoch))` (S20).
- SegmentDescriptor (catalog.rs:46-82) is **not** changed. weftdb wraps it as `IndexRow { desc, gen, prec, frame_crc, commit_epoch }`.

**`metadata.db`.** After S11 it is no longer opened or written. It is left on disk for downgrade, and backups stop including it.

**`.weftpart` sidecar v4.** Today it is version 3 behind magic byte 0x02 (weftdb/src/types/partial_sidecar.rs:38, 53).
- v4 uses magic last byte 0x03 plus a CRC-32 trailer over magic and body.
- The stamp becomes `(row_count, byte_len, frame_crc)`, replacing today's `(row_count, byte_len)` (partial_sidecar.rs:235-237).
- v3 files fail the magic check and are treated as absent (partial_sidecar.rs:222-229).

**Backups.** A backup is `backup-<ms>/{MANIFEST.json, segment_index.db, aspect_catalog.db, catalog.db, segments/}`.
- It is built under a `.partial-*` name and becomes valid only through the final rename.
- Pre-S16 directories (no frames) and pre-S5 directories (four DBs, no manifest) remain restorable as control-plane-only.

**Legacy stores:**
- `aspects.storage_mode TEXT DEFAULT 'rows'`.
- A seal-backed aspect's frames live in a Storage-v2 root at `{data_dir}/{db}/segments_v2/`, scope `(db, 'legacy')`, keyed by the AspectId UUID.
- `Database::new` builds under `.{name}.creating-{nonce}` (S18).

**Compatibility.**
- A v1 store migrates in place idempotently, with no data rewrite.
- Downgrade is safe only before the first gen ≥ 1 frame exists: an old binary would go back to `MAX(id)+1` and in-place rewrites. Release notes say so, and new binaries refuse a `layout_version` newer than they know.

---

## 5. Commit protocols

### 5.1 Shared machinery

- **AspectLocks** (S7), per aspect:
  - `commit: tokio::Mutex<AspectState{next_id, epoch}>`, held only around one control-plane transaction;
  - `maint: tokio::Mutex<()>`, held for a whole maintenance operation;
  - `next_gen: AtomicU64`, lock-free.
  - Seeding happens at open; see R10.
- **IndexTxn** (S6) runs a list of ops in one `BEGIN CONCURRENT … COMMIT`.
  - Ops: InsertNew (plain INSERT, never OR REPLACE as at segment_index.rs:153), ReplaceExpected and DeleteExpected (each must change exactly one row matching `(aspect,id,gen,frame_crc IS ?)`), JournalPending, JournalRetire, ClearJournal, LedgerInsert, LedgerInvalidateSpan, QuarantineRow, RollupFold/RollupDelta, ChangeAppend, SeqBump.
  - Error classes:
    - **Retryable:** Busy, BusySnapshot or WriteWriteConflict raised before COMMIT (or by COMMIT's own validation, which rolls back before writing the log). Retried up to 5 times with backoff when every op is guarded (InsertNew, ReplaceExpected, DeleteExpected), or when the conflict came before any op ran. The legacy INSERT OR REPLACE and delete-by-id ops that seals and maintenance commit until S7-S9 are not retried once they ran: a retry would replay them over the winner's commit.
    - **Conflict:** a precondition failed.
    - **Definite:** any other error before COMMIT.
    - **Ambiguous:** an error returned by COMMIT itself.
- **Poison** (S6). An Ambiguous result, or an fsync error on shared state (the DirSyncer), sets `store.poisoned`.
  - Every write entry point then returns `Poisoned` (HTTP 503). Reads continue, and `/ready` reports `restart_required`.
  - With `WEFT_ON_AMBIGUOUS_COMMIT=exit`, the process logs and exits with code 70.
  - Restarting runs recovery. Turso's log replay is the authority on whether the transaction committed.
- **DirSyncer** (S2). A single in-flight `fsync(segments/)` with generation counters. A caller returns only after an fsync that began after its own create finished, so N concurrent writers cost at most N fsyncs and usually one.
- **Reclaim** (S8).
  - `pin()` at the top of every read path (read_time_range, downsample_range, read_point, read_points, read_value_range, decode_all callers).
  - A swap stamps `retire_epoch = current` and bumps the epoch.
  - The reaper unlinks a retired frame only when no pin is older than its `retire_epoch` and the backup `gc_hold` is zero.

### 5.2 SEAL (S10; ingest entry point S13; HTTP S14)

**SEAL-1. Admission.** Runs in a detached task: the handler awaits the JoinHandle (S14), so a client disconnect cannot cancel a later step.
- Validate schema and tolerance, and enforce `require_sorted`. Errors are 400, as today.
- Compute `fingerprint = crc32(canonical(timestamps LE, values as plain text, rows_per_page)) ⊕ (row_count, min_ts, max_ts)`.
- If the call has a key, read the ledger: same fingerprint → replay (200); different → 409.

**SEAL-2. Encode.** Runs on `spawn_blocking` when rows exceed `WEFT_SEAL_BLOCKING_ROWS` (65,536). Today the encode runs on a tokio worker (segment_store.rs:595, 618).
- Split into frames of at most `WEFT_INGEST_MAX_SEGMENT_ROWS` (1,048,576) rows.
- Encode each with today's `write_to_with` frame options (segment_store.rs:595).
- `frame_crc` = the trailing CRC of each frame.

**SEAL-3. Name.** `gen = next_gen.fetch_add(1)`, `prec = gen`. The name is never reused.

**SEAL-4. Write.** On `spawn_blocking`: `OpenOptions::create_new` the final name, `write_all`, then in strict mode `sync_all` (**FSYNC #1**, one per frame). On any error, remove this call's own files and return Definite (500 `committed:false`).

**SEAL-5. Directory.** In strict mode, `DirSyncer.sync()` (**FSYNC #2**, shared across writers and aspects). An error poisons the store and returns 500 `committed:false`; the frames are left for recovery.

**SEAL-6. Commit point.** Take `aspect.commit`, check `poisoned`, then `BEGIN CONCURRENT`:
1. If the call has a key, re-SELECT the ledger row. If it now exists: ROLLBACK, release the lock, unlink own frames, and return the stored outcome (this closes the concurrent same-key race).
2. Assign `id = next_id .. next_id+k-1`.
3. InsertNew each row (gen, prec, frame_crc, `commit_epoch = epoch+1`, root-joined path).
4. RollupFold (from S11; before S11, `record_seal` runs after COMMIT under the same lock).
5. LedgerInsert.
6. ChangeAppend (seal-backed legacy aspects, S20).
7. SeqBump(next_id, next_gen, epoch).
8. `COMMIT`. **This is the commit point.** Under FULL, Turso fsyncs the logical log (**FSYNC #3**), group-committed with commits on other aspects.

Error handling:
- Retryable → retry.
- Definite → unlink own frames and return 500 `committed:false`.
- Ambiguous → keep the frames, poison the store, return 503 `outcome_unknown`. Nothing is unlinked after COMMIT has been issued.

After success, update AspectState and release the lock.

**SEAL-7. Ack.** Return the receipt `{segment_ids, row counts, min_ts, max_ts, time_sorted, replayed:false}`. `segment_id` stays as the first id (weft-server/src/manage.rs:719-723).

**SEAL-8. Sidecar.** After the ack, on a detached bounded queue, materialize the sidecar to `.tmp-*` and rename it into place, with no fsync. Today this runs before the ack (segment_store.rs:603-608).

### 5.3 SWAP: every maintenance mutation (S8 reconcile/split, S9 overlap/squash/compact)

**M1. Lock and snapshot.**
- Take `aspect.maint`. The daemon uses `try_lock` and skips busy aspects. The HTTP endpoints (manage.rs:230-282, 337-377, 421-435, 474-486) wait up to 30 s, then return 409.
- Snapshot the input rows `(id, gen, frame_crc, desc)`.

**M2. Plan outputs.**
- Each output gets a fresh gen and `prec = max(member prec)`.
- **Precedence ids:** a target keeps the **min** member id, as today (segment_store.rs:1411, 1542, 1677). An overlap-split suffix takes the **max** member id; today it takes `next_id` (1416). A `split_segment` suffix (library/test-only, callers at segment_store.rs:2598-2667) gets an id assigned inside the swap transaction, and only if no row with a higher id overlaps `[boundary, max_ts]`; otherwise the split is refused.
- Why this is correct:
  - Every commit after the snapshot has an id above every member, because ids are assigned at commit.
  - Members are an overlap closure, or a contiguous id run in compaction (segment_store.rs:1642-1660), so no non-member that existed at the snapshot shares timestamps with the outputs.
  - Today's fold order, members ascending by id with newer wins (segment_store.rs:1380-1399, merge at weft-physical-type/src/split.rs:147), is preserved.

**M3. Pending journal.** Commit `frame_journal` 'pending' rows for every output name. This is a small FULL commit. It needs no lock, because the names are unique.

**M4. Build outputs.**
- Decode the inputs, checking `len == byte_len` and the CRC against `frame_crc`.
- Merge and encode on `spawn_blocking`.
- Write each output with `create_new` and `sync_all`. This fsync happens in **both** modes.
- `DirSyncer.sync()`.

**M5. Swap.** Take `aspect.commit` and run one IndexTxn:
- ReplaceExpected or DeleteExpected for every input, against its snapshot `(gen, frame_crc)`.
- InsertNew for outputs that take a new id. The `format_version` comes from the frame actually written, which removes the paged-to-single-block wedge.
- JournalRetire every input name with `retire_epoch`, and ClearJournal the outputs' pending rows.
- RollupDelta (S11), then SeqBump.
- `COMMIT` (FULL). This is **the commit point**.

On Conflict: roll back, then delete the pending rows and the outputs. A crash at that point leaves them for R6. On Ambiguous: poison the store.

This single transaction replaces today's separate commits at segment_store.rs:991, 1023 and 1424-1430, 1544-1550, 1679-1685, along with their trailing rebuilds.

**M6. Release.** Release the locks, hand the retired names to the reaper, and refresh sidecars for the outputs (detached).

### 5.4 REAPER (S8)

- **G1.** Select retired journal rows that are eligible under the reclaim pins and `gc_hold == 0`.
- **G2. Guard.** Skip, and log loudly, any name that a live row still references. This cannot happen by construction; it is defense in depth.
- **G3.** Remove the frame and its sidecar. NotFound counts as success. Before S15 introduces stem-named sidecars, an id-named sidecar is removed only when that id has no live row.
- **G4.** `DirSyncer.sync()`.
- **G5.** One commit deletes the processed journal rows.
- The same task prunes ledger rows past their TTL (S13) and quarantine entries past their TTL (S12).

### 5.5 OPEN, SHUTDOWN, POISON

**Open (S3).**
1. Take LOCK before opening any DB.
2. Open the four DBs. Probe `PRAGMA journal_mode` and fail the open unless it reports MVCC. Probe `PRAGMA synchronous` on a fresh connection and fail unless it is FULL. This replaces the `.ok()` at segment_index.rs:94, metadata.rs:158, catalog.rs:56 and aspect_catalog.rs:44.
3. Run the schema migration.
4. fsync `segments/` and the root once, which covers the DB files and their `-log` entries. The root's parent is fsynced on every open, so the root itself survives a power cut even when the open that created it died before this step; when this open created the root, the directories it created above it and the parent of the topmost one are fsynced too. A root that predates the open and sits in a parent the server cannot read is opened anyway, with a warning.
5. Register database and subject in **one** catalog transaction. Today these are two commits (segment_store.rs:289-290 → catalog.rs:119-160). This comes after the fsync, so the registration commit lands in a `-log` whose directory entry is already durable.
6. Run recovery (section 6).
7. The server binds its listener only after `build_state` returns (weft-server/src/main.rs:98-102), so no request is served before recovery finishes.

**Shutdown (S14).** `axum::serve(...).with_graceful_shutdown(SIGTERM/SIGINT)`; today there is none (main.rs:106). Then:
1. Stop the daemons through a watch channel.
2. Drain in-flight ingests.
3. Run the reaper.
4. Commit `clean_shutdown = true`.

### 5.6 Relaxed mode (S17)

- The seal skips SEAL-4's `sync_all` and SEAL-5's directory fsync.
- A syncer runs every `WEFT_SYNC_INTERVAL_MS` (1000). For each aspect it:
  1. takes the frames committed since its last pass,
  2. opens and `sync_all`s each one,
  3. calls `DirSyncer.sync()`,
  4. commits `aspect_seq.synced_epoch` = the highest covered epoch.
- Recovery CRC-verifies only rows with `commit_epoch > synced_epoch`.
- The watermark lives in the FULL-synced database. It is not inferred from a lock file.

---

## 6. Recovery (S12; a minimal journal replay ships earlier in S8)

Every step can be re-run. A crash at any step is followed by a full re-run, and the test plan crashes at each step.

**R1. Lock.** Take LOCK, or fail with "store root in use by pid N".

**R2. Open.** Open the DBs; Turso replays its logs here. Then run the probes and the migration, and read `store_meta`. **Restore mode** is on if the `RESTORED` marker exists.

**R3. Register.** Register the scope (one transaction).

**R4. List.** List `segments/` and `segments/quarantine/` and classify every entry:
- legacy frame (parse at the last `-`);
- new frame (parse `enc~g~p`);
- sidecar;
- `.tmp-*`;
- unknown (logged and left alone).

Record the maximum legacy id and the maximum gen per aspect, across both directories and across journal and quarantine rows.

**R5. Temp files.** Delete `.tmp-*` files.

**R6. Replay `frame_journal`.**
- 'pending' and unreferenced: unlink.
- 'pending' and referenced: drop the row.
- 'retired' and unreferenced by any live row: unlink the frame and its sidecar.
- 'retired' but referenced: keep the file and log it (the guard).

Then fsync `segments/`, and make one commit that deletes the processed rows.

**R7. Verify every live row.**
- Stat the resolved path. A missing frame, or a length different from `byte_len`, quarantines the row: one IndexTxn per aspect runs QuarantineRow, LedgerInvalidateSpan and a rollup recompute, and any file goes to `quarantine/`. Reads over that span then succeed with the remaining data, instead of every overlapping query failing as it does today at segment_store.rs:749, 883, 921 and 1818.
- CRC-verify against `frame_crc` for these rows:
  - in relaxed mode, rows with `commit_epoch > synced_epoch`;
  - all rows when `WEFT_RECOVERY_VERIFY=full`.
- In restore mode, rows with a missing frame are quarantined. Their data is normally in a newer frame that R8 adopts.

**R8. Unreferenced frames that are not journaled.**
- **Normal mode:** move to `quarantine/`. These can only be crash litter from seals that never committed, which the client never saw as a 2xx.
- **Restore mode:** CRC-valid frames are **adopted** in ascending `(prec, gen)` order. Legacy-named orphans are adopted first, in id order. For each one:
  1. rebuild the descriptor from the frame (catalog.rs:95, 107);
  2. assign a new id above every restored id;
  3. schedule an overlap reconcile for the aspect.

  Invalid frames go to `quarantine/`. With `WEFT_RESTORE_ADOPT=manual`, nothing is adopted automatically; frames are listed for `POST /api/v1/storage/quarantine/adopt`.

**R9. Sidecars.** Delete any sidecar that has no matching live frame, or that fails the v4 CRC or stamp check.

**R10. Seed allocators.**
- `next_id ≥ max(aspect_seq.next_id, MAX(id)+1, max legacy id seen on disk or in quarantine + 1)`
- `next_gen ≥ max(aspect_seq.next_gen, max gen seen + 1)`

Persist both in one commit.

**R11. Rollup.** Recompute the rollup when `rollup_migrated` is unset, in restore mode, or for any aspect that had a quarantine or adoption.

**R12. Purge.** Delete quarantine files and rows older than the TTL.

**R13. Finish.**
- If anything changed: fsync `segments/`, `quarantine/` and the root.
- Remove `RESTORED`.
- Set `clean_shutdown = false`.
- Publish a RecoveryReport (journal rows replayed, rows and frames quarantined, frames adopted, rollups rebuilt, duration) to the logs, to `/ready` (today unconditional, weft-server/src/lib.rs:134-137) and to `GET /api/v1/storage/fsck`.
- Start the reaper, the sidecar backfill and the daemons.

**Cost.** One `read_dir` plus one stat per row. A CRC pass runs only over the relaxed-mode tail or when explicitly requested. The CRC is byte-at-a-time (weftseg.rs:137-145), which is why a full CRC pass is opt-in.

---

## 7. Maintenance operations mapped to SWAP

| Op | Today | New |
|---|---|---|
| reconcile_segment (segment_store.rs:956-999) | O_TRUNC rewrite at 983/988, then INSERT OR REPLACE (991) | one output: same id, new gen, frame kind preserved; ReplaceExpected; old gen retired |
| split_segment (1064-1098, library-only) | suffix next_id (1091) then in-place prefix (1092-1093) | prefix: same id, new gen; suffix: id assigned in the swap transaction only if no higher-id overlap; one transaction |
| reconcile_overlaps full branch (1350-1440) | reseal into the lowest id, then a per-member delete loop (1424-1430) | target min id new gen; members DeleteExpected + retired, in one transaction |
| reconcile_overlaps split branch (1407-1418) | suffix next_id (1416), in-place prefix (1418) | prefix min id new gen; suffix **max member id** new gen; one transaction |
| squash_aspect (1524-1553) | reseal ids[0], delete loop (1544-1550) | one swap |
| squash_aspect_to_target_rows (1635-1693) | per-group reseal + delete loop (1679-1685) | one swap per group |

- `reseal_nullable_at` (1010-1027) is replaced by a write-only `write_output`.
- `refresh_sidecar_after_rewrite` (720-730) is no longer needed: nothing is rewritten, so outputs get fresh sidecars.
- The comments at 941-943, 1049-1051 and 1089-1090, and README.md:232 ("immutable once sealed"), are updated to describe the real guarantee.

**Sweep isolation (S4).** The store-wide sweeps stop propagating the first per-aspect error with `?` (segment_store.rs:1181, 1262, 1460, 1493, 1600, 1714, 1772). They collect `failed: Vec<(aspect, error)>`, and the daemon (weft-server/src/reconcile_daemon.rs:239-303) logs each failure. A deterministic frame error (ChecksumMismatch, UnexpectedEof, UnsupportedVersion, binding mismatch) quarantines the row through IndexTxn (S12); an I/O error is retried on the next tick.

**Rollup in swaps (S11).**
- Subtract the inputs' additive fields and add the outputs'.
- `time_range` cannot change, because merges keep every timestamp.
- `value_range` is recomputed from `load_index` only when an input held the current extreme; outputs' values are a subset of the inputs'.
- The result stays exactly equal to `from_index` without a scan on every swap.

**Cost.** The same encode and decode work as today, plus one fsync per output, one directory fsync, a pending commit and a swap commit. Today the same work takes 1+k unsynced-file commits. Transient space is up to 2x a component until the reaper runs, which is normally seconds; space held by backup links is reported.

---

## 8. Ingest routing

**One entry point (S13):** `SegmentStore::ingest(aspect, IngestBatch{timestamps, values: Vec<Option<BigDecimal>>, rows_per_page}, IngestOptions{idempotency_key, require_sorted}) -> IngestOutcome{descriptors, replayed}`.
- `seal`, `seal_nullable`, `seal_paged*` and `seal_declared*` (segment_store.rs:526-586) become thin wrappers, so the library API stays source-compatible.
- weft-arrow-store's Parquet path (weft-arrow-store/src/lib.rs:227-243) calls `ingest`.

**HTTP (S14):**
- All four formats go through it: JSON (manage.rs:789-867), CSV (960-994), ILP (1071-1129) and Parquet (weft-server/src/storage.rs:351-382), each in `tokio::spawn` with the JoinHandle awaited.
- The key comes from the `Idempotency-Key` header (visible ASCII, at most 128 bytes) or from `batch_id`, and is scoped per aspect. Today `x-request-id` only correlates traces.
- `IngestResponse` gains `segment_ids` and `replayed`. The additive JSON keeps `segment_id`.
- `classify_seal_error` (manage.rs:748-754) maps the new error classes.
- `DefaultBodyLimit::max(WEFT_MAX_INGEST_BYTES)` (default 256 MiB) applies to the ingest routes only. Today the router sets no limit (weft-server/src/lib.rs:124), so axum's default applies: 2,097,152 bytes (axum-core-0.5.6 src/ext_traits/request.rs:319; axum 0.8.9 in Cargo.lock).
- A byte-weighted semaphore, `WEFT_INGEST_MAX_INFLIGHT_BYTES`, returns 503 + Retry-After instead of queuing without bound.

**Batching guidance.** There is no server-side memtable in 1.0. Recommend at least 10k rows per request, and at least 100k on COW or spinning volumes. A WAL front end for small polling writes (7.1) is deferred.

**Legacy:**
- **S18, rows mode.**
  - `Database::new` builds under `.{name}.creating-{nonce}`, then renames and fsyncs `data_dir`. Today it runs `mkdir` and later commits with no cleanup (database/mod.rs:342-418).
  - `batch_capture_measurements` enqueues timestamps **before** the chunk loop. Today the enqueue runs after it (inputs.rs:330-333), so the queue is always a superset of the data.
  - `insert_unprocessed_batch` (inputs.rs:446-479) skips the insert when `(aspect_id, batch_hash)` already exists among unprocessed or processed batches. The hash is already computed (inputs.rs:449) and the schema invites application-level dedupe (aspect.rs:545-546). The processed batches live in a separate DB (database/config.rs:12).
  - As built: every creator holds a shared lock on `{data_dir}/.weft-creating.lock` from before it creates its build directory until after the rename, and a sweep removes `.creating-*` directories only while it holds the exclusive lock (it skips the sweep when the lock is busy), so a live build in this or another process is never swept. `new` refuses names of the build form. The on-disk listing is `Database::list_stored_databases`, which sweeps first. `L-new-created` is after the `database` row commits and before the rename; `L-new-renamed` is after the rename and before the `data_dir` fsync. Both are process-crash points only, until `SimFs` models directories (section 11). The rename is the commit point, so after a crash at `L-new-renamed` the retry is `existing(name)`, not `new(name)` (which reports that the database exists). Nothing after the rename is undone either: a failed `data_dir` fsync (or an error returned at `L-new-renamed`) leaves the complete database in place and returns an error saying it was created, may not survive a power loss, and opens with `existing`. Renaming it back and removing it, as the first version did, deleted files under any handle a concurrent `existing(name)` had opened in between. The publish (fsync, rename, `data_dir` fsync, and on failure the rename back and the cleanup) runs as one blocking task that holds a share of the creator lock: a `new` future dropped mid-publish leaves the directory to that task, never removing files under an in-flight rename. After the publish the database exists, so a failure to open it at its final path is reported as such (open it with `existing`), and the checkpoint is best-effort.
  - As built: `capture_measurement` also enqueues first. Every step after a commit is best-effort (logged, the call returns `Ok`): the second enqueue below, the checkpoint, the dirty-region marking, the earliest/latest update and `capture_measurement`'s transaction-log entry.
  - As built, ingest concurrent with the consumer: the write-ahead entry is visible before its row commits, so a consumer can read it, build the window without the row (inside the stored range `analyze_range` interpolates across the gap; before it there is no window and the consumer clears what it read) and dequeue it. So ingest enqueues each chunk's timestamps (`capture_measurement`: its one timestamp) again after the commit, enqueuing an already-queued timestamp moves its `queued_at` to `MAX(now, queued_at + 1)`, and the consumer reads `UnbatchedEntry`s and dequeues only those, matched on `(data_timestamp, queued_at)` (`dequeue_unbatched_entries`, one prepared DELETE per entry because Turso seeks the unique key for `=` but scans the aspect for `IN`). A second enqueue after the consumer's read survives its dequeue; one before its dequeue re-adds the entry. Because the enqueue is an upsert it writes already-queued entries, so it conflicts (MVCC write-write) with a consumer's DELETE of the same entry or another ingest's enqueue of the same timestamp: both of ingest's enqueues and each dequeue transaction retry a conflict (10 attempts, backoff doubling from 20 ms to 500 ms, about 3 s), and the consumer dequeues in transactions of at most 5,000 entries (one synced commit each, where a run used to dequeue in one), so an enqueue waits for one of them. The exception is the write-ahead enqueue, which upserts all of a `batch_capture_measurements` call's timestamps in one transaction: a dequeue of some of the same timestamps (a re-import of still-queued timestamps) waits for all of it, and either side can exhaust its retries (the consumer run or the import fails; both are safe to re-run). `capture_new_measurement`, `capture_measurement_chunk` and `capture_new_measurement_chunk` have no write-ahead enqueue (outside S18's scope); their post-commit enqueue retries conflicts the same way, and still fails the call after its rows are stored when it cannot write. A consumer that batched a window from rows already committed rebuilds it after the second enqueue into a byte-identical batch, which the batch dedupe below skips even once extracted. The consumer aligns its windows on the earliest measurement read uncached on every run (`get_earliest_measurement_uncached`): the cache is per `Database` instance (10 minutes), and a stale value, from this or another instance, put a backfill before the consumer's base, where the consumer cleared it from the queue. Ingest still drops its own instance's cached value after each commit, for other readers of that instance. The consumer also reads the latest stored measurement (after the entries) and skips every window that ends after it without calling `analyze_range`, which cannot fill it; otherwise the write-ahead entries of an abandoned append, past the stored range, cost every later run one futile window build each. A row committed after that read is queued again after it, so its entry survives the dequeue. Residuals: a row stays unqueued only if a consumer ran during the call and the call then died, or exhausted its retries, between a commit and the second enqueue. A consumer run between two chunks of a backfill or gap fill (rows inside the stored range, or before it with chunks still to come between the committed ones and the stored rows) batches the windows spanning committed rows and rows still to come with the latter interpolated, and the next run batches them again with all their rows: two different batches, and occurrences, for one window, which the dedupe keeps (the accepted "two batches for one window" of the decisions; past the stored range a window short of points is not batched, so appends are unaffected).
  - As built, batch dedupe: processed batches keep the `batch_hash` they were queued under, since processing rewrites the measurements and a recomputed hash would never match. Pattern extraction removes the batches it consumed with `remove_extracted_batches`, which records each one's `(aspect_id, batch_hash)` in `extracted_batches` (processed batches DB) in the transaction that deletes it; `clear_processed_batches` (a full rebuild) clears that record with the batches, so a full rebuild still extracts every window again. The record grows by about one row per resolution step (extraction consumes one sliding-window batch per step), so `remove_extracted_batches` also deletes, in the same transaction, the rows older than a retention (`WEFT_EXTRACTED_BATCH_RETENTION_SECS`, default 48 hours): the rebuilds it guards against come on the first consumer run after a requeue, or on the re-run after a consumer crash, so the retention must exceed the interval between pipeline runs plus an ingest call. Residual: a rebuild after the record expired (a pipeline run, or a crash recovery, more than the retention after the extraction) extracts the window again. The record has no `aspect_id` (the DB is per aspect) and is indexed on `batch_hash`. Both batch tables get a best-effort, non-unique `idx_batches_hash` on `(aspect_id, batch_hash)`, and `extracted_batches` `idx_extracted_batches_hash` (on `batch_hash`), created when a process first opens them (Turso 0.8 maintains indexes under MVCC; the measurements' timestamp index is created the same way), so each check is a point lookup. The checks follow a batch's way through the tables: unprocessed first, inside the insert transaction, then processed, then extracted. `move_batches_to_processed` inserts into processed before deleting from unprocessed, and the extracted record is written with the delete from processed, so a batch moving between two checks is seen by the later one. So a consumer that rebuilds windows it already batched (after a crash before its dequeue, or after an ingest that ran during it queued its rows again) queues none of them again, whatever processing and extraction did in between, within the record's retention.
- **S19, seal-backed aspects.**
  - `storage_mode = 'segments'` is chosen at aspect creation, or for every new aspect with `WEFT_LEGACY_DEFAULT_STORAGE=segments`. Existing rows-mode aspects are never switched implicitly. There are no union reads.
  - On first use the adapter declares `Decimal128` at zero tolerance (weft-physical-type/src/lib.rs:137-143). If a value would be lossy, it widens the declared encoding to `BigDecimalText` (lib.rs:144-149) for later frames. Frames self-describe, so nothing is silently downcast.
  - `batch_capture_measurements` calls `ingest` with the derived key `legacy:{dataset_id}:{payload_crc:08x}:{n}`. The whole call is one transaction, so it is all-or-nothing, and a retry is a replay. It returns synthetic TxIds (UUIDv5 over key and row index); the only production caller ignores them (weft-tui/src/app.rs:2189).
  - `capture_measurement` becomes a one-row ingest, followed by an inline `squash_aspect_to_target_rows_if_fragmented`; that gate is O(1) (segment_store.rs:1744-1750).
  - The legacy readers for seal-backed aspects go through SegmentStore: raw, chunk, range, count, boundary, and earliest/latest. These are the only SQL sites: outputs.rs:138, 153, 204, 1348-1357, 1383, 1403 and database/mod.rs:1051, 1095.
  - Aspects with a `compression_config` (aspect.rs:105) stay in rows mode, because compression deletes ranges (inputs.rs:1579, 1596).
- **S20.** Seal-backed aspects drop the per-timestamp queue (inputs.rs:23-59). The publish transaction appends one `segment_changes` row. The consumer (weft-orchestration/src/batch_utils/load_unprocessed_batch_queue.rs:41-129) reads changes past a watermark stored in `unprocessed_batches.db`, and advances that watermark in the same transaction as its batch inserts. This replaces the separate dequeue at load_unprocessed_batch_queue.rs:122.

---

## 9. Backup and restore

**Directory hygiene (S5; applies to today's control-plane backup as well):**
1. Build in `base/.partial-{label}-{nonce}/`.
2. Verify each snapshot, including its expected table set; today an empty `catalog.db` passes, because backup.rs:216-231 accepts zero tables.
3. Write `MANIFEST.json` with `create_new` + `sync_all`, then fsync the directory.
4. Rename to the final label and fsync the base. Only then report success and prune.

Retention and cleanup:
- **Counting.** Retention (weft-server/src/backup_daemon.rs:100-121) counts `backup-<digits>` directories that hold a manifest, plus legacy directories that hold all four DB files.
- **Prune** renames each victim to `.deleting-*` and fsyncs the base before `remove_dir_all`; today it removes in place (backup_daemon.rs:136-137).
- **Sweep.** `.partial-*`, `.deleting-*` and `.restore-drill-*` older than one hour are removed at daemon start and on every tick.
- **Drill.** It reports `cleanup_error` instead of discarding it with `let _` (manage.rs:551).
- **Restore.** `restore_control_plane` (backup.rs:381-406) copies each file to a tmp name, `sync_all`s it, renames it, then fsyncs the root.

**Whole-store backup (S16), with no ingest barrier:**
- **B1.** Increment `gc_hold`, which pauses only the reaper.
- **B2.** `VACUUM INTO` `segment_index.db` **first**. It is the authority: rows, rollup, allocator and ledger. Then `aspect_catalog.db` and `catalog.db`; declarations only grow. Each VACUUM INTO target is fsynced by Turso (vdbe/vacuum.rs:366-380).
- **B3.** Open the snapshot copy and hard-link every frame it references into `.partial/segments/`. On EXDEV, copy and `sync_all` instead.
- **B4.** Release `gc_hold`.
- **B5.** Write the MANIFEST: DB files, frames as `(name, byte_len, frame_crc)` (a legacy row's CRC is read from the trailer), the allocators and `store_uuid`. Then fsync.
- **B6.** Rename and fsync the base.

Consistency follows from three facts: swaps are single transactions, frames are immutable, and the reaper cannot unlink anything the snapshot references.

**Restore (S16):**
- **`restore_store(backup, new_root)`.** Requires a MANIFEST. Stages into `{new_root}.restoring-*` with every file and directory fsynced, renames it into place and fsyncs the parent. The first open runs normal recovery. Readers resolve paths against the current root (S6), so the restored store reads its own frames. Today the round-trip test at segment_store.rs:2204-2241 passes only because the original root still exists.
- **`restore_in_place(backup, root)`.** This is the shape documented at backup.rs:370-374. With the server stopped (LOCK free):
  1. move the current DBs to `root/.corrupt-{ts}/`;
  2. copy and fsync the snapshot DBs;
  3. write and fsync `RESTORED`.

  The next open runs recovery in restore mode. Rows whose frames were reaped after the backup are quarantined. Post-backup frames are adopted in `(prec, gen)` order, so happens-before is preserved; concurrent writes keep no defined order. Allocators are seeded above everything on disk, so no post-backup frame can ever be overwritten.
- **The drill** becomes a whole-store restore using links: open, optionally CRC-verify the frames against the manifest, then remove.

**Limits.**
- Same-filesystem links share inodes, so they protect against control-plane loss, operator error and logical corruption, but not media loss. Put `WEFT_BACKUP_DIR` on another device to get copies; the response states linked or copied.
- Retained backups pin superseded frames; that space is reported.
- RPO = the backup interval. No PITR.

---

## 10. Performance

**Baseline.** 1M real rows seal in 4.57 s: 218,963 rows/s at 4.22 B/point. Legacy runs at 2,978 rows/s (n=20k) and 1,447 rows/s (n=40k) (ROADMAP.md:1134-1136). That seal figure already includes **two** FULL log fsyncs per seal: segment_index COMMIT (segment_index.rs:161) and metadata.db COMMIT (metadata.rs:250 via segment_store.rs:600). It was measured in a `tempfile::tempdir()` (weftdb/tests/db_tests.rs:670, 696). `/tmp` on this box is tmpfs (findmnt), so fsyncs were effectively free. The store volume is btrfs on /dev/sdd (findmnt), where backup ticks took about 1.0-2.5 s under load (ROADMAP.md:925-936).

**fsync-class operations per seal:**

| Mode | Isolated seal | G concurrent seals (different aspects) |
|---|---|---|
| Today | 2 log fsyncs | 2 per seal; group commit may coalesce |
| Strict (after S11) | 1 frame + 1 dir + 1 log = 3 | 1 + 2/G (one DirSyncer fsync, one group-committed log fsync) |
| Relaxed | 1 log | 1/G |

Ack-path changes:
- **Removed from the ack path:** the metadata.db commit (S11) and the sidecar decode, reduce and write (S10).
- **Moved off tokio workers:** the encode.

Expected results, to be measured in S10 and S21 rather than assumed:
- **NVMe ext4/XFS** (a few ms per fsync): under 1% added at 1M-row requests, and roughly 5-10% at 10k-row requests.
- **Loaded btrfs HDD** (budget 20-500 ms per fsync): about 2-30% at 1M rows. Small single-stream requests are fsync-bound at about 5-20 per second.

**Per-aspect cap.** Commits on one aspect are serialized by `aspect.commit` and pay their own log fsync, so each aspect does at most about 1/log-fsync-latency commits per second. That is 15-50/s on loaded btrfs and hundreds per second on SSD. It caps request rate, not rows/s, at bulk sizes. The follow-up, if S10's benchmark shows a need, is for the lock holder to drain every queued intent for its aspect into one transaction; that is per-aspect group commit with no store-wide serialization point.

**Maintenance.** Background cost: one fsync per output, one directory fsync and two small commits.

**Recovery.** One stat per row; CRC only over the relaxed tail or when asked.

**Backups.** O(#segments) hard links. No ingest pause, unlike Atomic Publish's publisher barrier.

**Legacy.** Seal-backed aspects target about 68-199x, the seal-vs-legacy ratio at ROADMAP.md:1134-1136, only once S20 removes the per-timestamp B-tree queue writes (inputs.rs:23-59). S19 alone is bounded by that queue, and its benchmark must say so.

---

## 11. Test plan

**Harness (S2).** A `fault-injection` cargo feature, off by default, provides:
- `fault::hit(FaultPoint)` with three actions:
  - **ReturnErr** (the store then drops and reopens, which is exactly a process crash);
  - **Abort** (re-exec with `WEFT_FAULT=<point>:abort`, then `std::process::abort()`);
  - **Pause(Notify)** to force interleavings.
- `StoreFs` with RealFs and **SimFs**. SimFs tracks durable versus volatile file content and directory entries. `power_cut(seed)` materialises a legal post-crash image:
  - an unsynced file comes back absent, empty, as a prefix, as a prefix plus zero-fill, or complete;
  - each unsynced directory op is applied or not.

  Turso files are copied at the cut. That is sound because every returned COMMIT was FULL-fsynced (asserted in S3). Phantom commits come from a fault placed after COMMIT executes.

  Every other write is distrusted unless it went through `StoreFs`. `SimFs::new` records the files under the root as the durable baseline. A file created, rewritten or removed after that by anything else (today's `tokio::fs::write` seal frames, in-place reconcile and split rewrites, sidecars) gets the outcomes of an unsynced file, with no ordering. Only paths the test exempts are copied as they are; `SimFs::turso_file` exempts Turso's `*.db`, `*.db-log`, `*.db-wal` and `*.db-shm`. This is what lets the S8/S9, S10 and S15 regression tests below fail before their fix.
- `StoreFs::create_new_write` performs `create_new`, `write_all` and, under `SyncPolicy::Full`, `sync_all` on one handle in one `spawn_blocking` (SEAL-4, M4). Its `WritePoints` hit S-frame-created / S-frame-written / M-output-written between the steps through `fault::hit_blocking`. A ReturnErr there makes the write remove its own file, so it is not a process crash; use Abort for the process-crash matrix at those points.
- **Directories (extended in S5).** S2's `SimFs` did not model directories. S5 keys every entry by the inode of the directory holding it, so `create_dir`, a directory `rename` and `remove_dir_all` (one unlink or rmdir per entry) are pending operations in the parent, durable once the parent is fsynced, and a directory carries its subtree: entries inside a directory whose own entry is lost are lost with it, and entries made after an unsynced rename follow the directory to whichever name survives. A directory made behind `SimFs`'s back is as untrusted as a file. B-partial-created, B-renamed, prune-renamed, restore-renamed and L-new-renamed, and the backup-dirent-not-durable window, can therefore be power-cut simulated.

**Fault points:**

| Group | Points |
|---|---|
| Seal | S-encoded, S-frame-created, S-frame-written, S-frame-synced, S-dir-synced, S-txn-begun, S-rows-inserted, S-commit-phantom, S-committed-unacked, S-mid-sidecar; plus ingest between-frames |
| Maintenance | M-planned, M-pending-committed, M-output-written, M-output-synced, M-dir-synced, M-swap-begun, M-swap-phantom, M-swapped |
| Reaper | G-unlinked, G-dir-synced, G-journal-deleted |
| Recovery | each of R1-R13 |
| Backup / restore | B-partial-created, B-vacuum(k), B-links, B-manifest, B-renamed, prune-renamed, prune-removed, restore-copied(k), restore-renamed |
| Legacy | L-enqueued, L-chunk(k), L-sealed, L-consumer-batches, L-new-created, L-new-renamed |
| Open | O-scope-database-inserted (S3: between the two inserts of `register_scope`) |

**Process-crash matrix.** Each point × each applicable op, where the ops are seal (dense, nullable, paged), multi-frame ingest, reconcile, split, overlap-full, overlap-split, squash, compact, reaper, recovery itself, backup, prune, restore, legacy ingest and `Database::new`. Each case uses ReturnErr, plus Abort for a sampled subset.

**Power-loss matrix.** The same points with `power_cut` on 16 fixed seeds in CI, and 256 seeds nightly. After every reopen, I1-I10 are checked.

**Regression tests that fail on main** (each slice must include its own):
- 64 concurrent seals lose an acknowledged batch (S7);
- reconcile racing squash resurrects a member (S7);
- 200 concurrent seals produce a wrong rollup (S7);
- power loss during reconcile/merge/split loses rows (S8/S9);
- a seal racing a two-member squash is lost (S9);
- an overlap-split suffix outranks a later-committed seal (S9 and S10);
- an acknowledged frame is not durable under SimFs (S10);
- relocating the root breaks reads (S6);
- acknowledged legacy metadata.db commits are lost on reopen (S1);
- a zero-filled sidecar tail is served (S15);
- a restore beside newer frames overwrites them (S16);
- an idempotent retry duplicates rows (S13);
- a 10 MB CSV body is rejected (S14).

**Other suites:**
- **Unit:** the R4-R9 classification table (row present/absent × file gen <,=,> × journal state × quarantine); `enc` injectivity under case folding (a property test); every sidecar truncation length and zero-fill offset rejected.
- **Concurrency:** a tokio multi-thread runtime, 256 ingests over 4 aspects plus daemon ticks, HTTP reconcile/squash/compact, backups and readers for 30 s. Checks: no acknowledged loss, I5, I7, readers never error, zero unexplained MVCC conflicts (a metric).
- **HTTP:** replay/409; body limit; a disconnect mid-ingest leaves the batch committed-and-replayable or fully invisible; SIGTERM drain; ambiguous COMMIT → 503 then 503 poisoned.
- **Subprocess:** `weft-server/tests/crash_kill9.rs` SIGKILLs the binary at random delays during keyed ingest and maintenance, restarts it, and verifies over HTTP.
- **Compatibility fixtures:** a checked-in pre-v2 store (absolute paths, `{aspect}-{id}` names, v3 sidecars), plus damaged variants (missing, zero-length or torn frame; orphan; v6 row over a v5 frame). Each opens, quarantines exactly the damaged rows and serves the rest.
- **Optional nightly privileged job:** dm-log-writes or dm-flakey on ext4 and btrfs, to validate the SimFs model.

**Running.** Run per crate with filters, e.g. `CARGO_TARGET_DIR=$HOME/.cache/weft-target cargo test -p weftdb --features fault-injection <filter>`, then `-p weft-server`. The workspace suite stalls in the splimes/database interpolation tests.

---

## 12. Window coverage map

| Window | Closed by (slice) | Mechanism |
|---|---|---|
| seal-orphan-frame-before-index-commit | S7, S10, S12, S14 | ids never shared (S7) and assigned at commit (S10); create_new with never-reused gen names, so no seal can truncate another's frame (replaces segment_store.rs:597/620); plain INSERT replaces OR REPLACE (segment_index.rs:153); a pre-commit error unlinks the call's own frames; crash litter is quarantined with a TTL (S12) and allocators are seeded above it; detached task (S14) |
| seal-index-committed-rollup-not-folded | S10, S11 | S10: a fold failure never becomes a 500 (manage.rs:748-754 path removed); S11: the rollup is in segment_index.db and folded in the same transaction, so no state has the row without the fold |
| seal-committed-unacknowledged | S13, S14, S10 | the ledger row commits with the descriptors, so a retry replays the receipt; detached ingest task; graceful shutdown (main.rs:106); sidecar off the ack path (segment_store.rs:603-608); keyless retries are documented at-least-once |
| seal-acked-frame-not-durable | S10, S3, S12 | frame sync_all and DirSyncer finish before COMMIT; DB directory entries fsynced at open (S3); legacy rows whose frame is missing or short are quarantined instead of failing reads (segment_store.rs:749/883/921) |
| sidecar-torn-write-process-crash | S10, S15 | sidecar written after the commit and the ack; tmp + rename; CRC; backfill |
| sidecar-powerloss-zero-tail-no-crc | S15 | v4 CRC-32 trailer over magic and body; today only the magic, postcard decode and version are checked (partial_sidecar.rs:207-229) |
| seal-or-rewrite-crash-before-sidecar | S8, S9, S11, S15 | rewrites produce new frame names, and sidecars are bound to the frame stem and frame_crc, so the (row_count, byte_len) collision (partial_sidecar.rs:235-237) cannot serve a stale partial; rollup inside the swap transaction; backfill |
| orphan-sidecar-matches-reused-id | S7, S8, S12, S15 | ids never reused; reaper unlinks the sidecar of a deleted id; R9 deletes unmatched sidecars; stem names + frame_crc stamp |
| inplace-reconcile-torn | S8 | output to a new gen, fsynced; ReplaceExpected swap; old gen retired; no O_TRUNC of a live file (removes 983/988) |
| inplace-reseal-torn | S9 | reseal_nullable_at (1010-1027) removed; all outputs written under new names; members are untouched until the reaper runs after a durable swap |
| powerloss-inplace-reconcile | S8, S2 | output and directory fsynced before the swap COMMIT; the old gen is unlinked only by the reaper after it |
| powerloss-merge-members-deleted-target-not-durable | S9 | member deletes, target update and retire rows share one transaction, which commits only after the outputs and directory are durable; the reaper fsyncs the directory before deleting journal rows |
| powerloss-split-halves-unordered | S8, S9 | both halves fsynced, then one directory fsync, then one swap transaction; neither half is reachable before it |
| rewrite-written-before-index-swap | S8, S9 | outputs carry new names and stay invisible until the swap; the pending journal + R6 delete uncommitted outputs |
| merge-paged-target-format-wedge | S9, S10, S4 | format_version comes from the written frame and commits with the new name; reader binding check on len and CRC; sweep isolation |
| split-duplicate-window | S8, S9 | suffix insert and prefix replace in one transaction (today 1092-1093 and 1417-1418 are separate) |
| split-suffix-orphan | S8, S9 | pending journal + R6; the overlap-split suffix reuses the max member id, so nothing is allocated; split_segment's suffix id is assigned inside the swap transaction |
| merge-target-committed-members-not-deleted | S9 | one transaction replaces the loops at 1424-1430, 1544-1550 and 1679-1685 |
| member-index-deleted-file-not-removed | S8, S9 | retired journal row in the swap transaction; reaper unlinks, fsyncs the directory, then deletes the row; R6 replays; live-row guard |
| maintenance-rollup-not-rebuilt | S8, S9, S11 | interim: one rebuild after each swap under the commit lock; final: delta fold inside the swap transaction |
| race-next-id-collision | S7, S10 | persisted per-aspect allocator under aspect.commit; plain INSERT; from S10 the id is assigned inside the commit transaction; unique gen names make even a hypothetical id clash harmless to files |
| race-max-id-reused-between-delete-and-unlink | S7, S8, S9 | ids never reissued; unlinks by exact retired name through the reaper, never by segment_path(aspect,id) (today 1426-1427) |
| race-reconcile-resurrects-merged-member | S7, S8, S9 | aspect.maint serializes the daemon and the HTTP endpoints; the swap precondition on (gen, frame_crc) aborts stale ops; no INSERT OR REPLACE remains |
| race-metadata-rollup-lost-update | S7, S11 | the fold runs under aspect.commit (S7) and inside the transaction (S11); no unlocked get-then-put (metadata.rs:270-271) |
| legacy-open-deletes-mvcc-logical-log | S1 | delete -log only when it is zero-length (database/mod.rs:80-86), mirroring backup.rs:187-200; the cold-open path holds the connection-cache lock from the cache check through the sweep, build and insert, so two in-process cold opens cannot unlink each other's live log. Residual: the empty-log unlink cannot see a live handle it does not own, i.e. a `turso::Database` clone that outlived `close()`/`clear_connection_cache_by_name`, or another process holding the DB open (its fcntl lock fails our open only after the sweep); unlinking a live empty log there orphans the commits that follow. Turso treats a zero-length log as absent (storage/journal_mode.rs:58-62), so dropping the unlink would close this at the cost of leaving empty logs on disk |
| legacy-batch-partial-prefix | S19 (seal-backed); residual for rows mode | seal-backed: one transaction for every frame, plus a derived idempotency key; rows mode: documented residual, see decisions |
| legacy-measurements-committed-not-queued | S18, S20 | rows mode: write-ahead enqueue before the chunk loop; seal-backed: the change row is in the publish transaction |
| legacy-committed-before-ack | S19, S18 | seal-backed: Ok directly after COMMIT, earliest/latest derived from the rollup, retry replays; rows mode: post-commit steps stay best-effort, and retry duplicates are a residual |
| legacy-capture-measurement-tail | S18, S19 | rows mode: enqueue first (the queue is a superset); seal-backed: a one-row ingest with nothing after the commit |
| legacy-queue-consumer-batch-before-dequeue | S18, S20 | rows mode: batch_hash dedupe inside the insert transaction; seal-backed: the watermark advances in the same transaction as the batch inserts |
| legacy-database-new-half-created | S18 | build under .creating-*, rename, fsync data_dir; stale .creating-* directories swept |
| legacy-power-loss-inflight-chunk | S1, S19 | acknowledged metadata.db commits now survive; seal-backed aspects use the strict single publish; a rows-mode in-flight chunk is never acknowledged (residual: durable unacknowledged prefix) |
| open-scoped-register-database-then-subject | S3 | register_scope in one catalog transaction |
| backup-vacuum-into-partial-dest | S5 | .partial build directory; manifest-last; table-set verification |
| backup-partial-dir-counts-toward-retention | S5 | retention counts only manifested or complete directories |
| backup-dirent-not-durable | S5 | directory fsyncs, rename, base fsync; prune via a fsynced rename to .deleting-* |
| backup-cross-db-skew-vs-seal | S11, S16 | the rollup lives in segment_index.db, so metadata.db is not backed up; segment_index is snapshotted first and is the authority |
| backup-captures-mid-maintenance | S8, S9, S16 | swaps are single transactions; frames referenced by the snapshot are linked under gc_hold |
| restore-beside-newer-frames-id-reuse | S7, S10, S12, S16 | allocators seeded above every id and gen on disk; create_new means no overwrite; orphans quarantined, not deleted; restore mode adopts them |
| restore-into-new-root-reads-old-paths | S6, S16 | readers resolve against the current root; backups carry frames |
| prune-partial-remove | S5 | rename to .deleting-* + fsync before remove_dir_all; sweep |
| drill-dir-leak | S5 | sweep .restore-drill-*; report cleanup_error (manage.rs:551) |
| verify-sidecar-litter | S5 | verification happens inside .partial, which is either published or swept |

---

## 13. Slice plan and milestones

Ordering rule: after every slice the store is no worse than today, and every slice ships with a regression test that fails on main where one is possible.

| # | Slice | Est h | Depends on |
|---|---|---|---|
| S1 | Preserve legacy MVCC logs; drop no-op pragmas | 2.5 | none |
| S2 | Durable I/O layer, fault points, SimFs | 8 | none |
| S3 | Storage-v2 open hardening, root LOCK, register_scope | 4 | S2 |
| S4 | Store-wide sweep isolation | 4 | none |
| S5 | Backup directory atomic publish and hygiene | 7 | S2 |
| S6 | Schema v2, IndexTxn, poison, root-relative path resolution, durable MVCC header (section 1.3) | 7 | S3 |
| S7 | Persistent allocator, per-aspect locks, plain INSERT | 6 | S6 |
| S8 | Write-once maintenance I: journal, swap, reaper, pins (reconcile, split) | 8 | S7 |
| S9 | Write-once maintenance II (overlap, squash, compact) | 8 | S8 |
| S10 | Write-once durable seal; id at commit | 8 | S9 |
| S11 | Rollup into segment_index.db | 7 | S10 |
| S12 | Recovery, quarantine, fsck, /ready | 8 | S11 |
| S13 | Idempotent atomic ingest (library) | 7 | S12 |
| S14 | HTTP ingest wiring and graceful shutdown | 7 | S13 |
| S15 | Sidecar v4 | 5 | S10 |
| S16 | Whole-store backup and restore modes | 8 | S5, S12 |
| S17 | Relaxed durability mode (optional) | 6 | S12 |
| S18 | Legacy rows-mode hygiene | 5 | S1 |
| S19 | Seal-backed legacy aspects | 8 | S13, S18 |
| S20 | Change log replaces the per-timestamp queue | 7 | S19 |
| S21 | Crash-matrix CI, kill soak, benchmark re-baseline | 7 | S14, S16 |

Interim states worth knowing:
- **S7 to S9:** seals still write legacy-named frames with unique ids, so there is no clobbering.
- **S10 to S12:** crash orphans from new-format seals leak until S12 quarantines them. That is a leak, not a correctness regression, so ship S12 promptly.
- **S7 to S9:** a seal that allocated its id before a maintenance snapshot but committed after it can still be outranked. This is pre-existing (segment_store.rs:1416) and is closed in S10.

---

## 14. Deferred past 1.0

- A WAL front end (SNW's group-committed log of pre-encoded frames), per aspect, for small polling writes (ROADMAP 7.1).
- Per-aspect intent batching, if benchmarks show the per-aspect commit cap binds.
- PITR and incremental backups; background scrub and repair; frame_crc backfill of legacy rows outside backups.
- A layout-kind byte in `.weftseg`, needed before any format bump, because the v5 and v6 version spaces are dispatched by descriptor (segment_store.rs:1819).
- A legacy `measurements.db` to segments migration tool, and union reads; compression on seal-backed aspects.
- Multi-process roots; directory durability on Windows.
- A change feed for non-legacy consumers.
- Exact commit-order reproduction of concurrent writes during adoption; adoption preserves happens-before only.

---

## 15. Risks

1. **Turso MVCC assumptions** ("experimental MVCC" is the accepted known risk, ROADMAP.md:782):
   - (a) a multi-table, multi-statement BEGIN CONCURRENT commits atomically (precedent: catalog.rs:244-256);
   - (b) conflicts are row/key-level, so commits on different aspects never conflict;
   - (c) ALTER ADD COLUMN works outside BEGIN CONCURRENT;
   - (d) a COMMIT that returns an I/O error may still be replayed later.

   S6 and S7 pin (a)-(c) with tests. (d) is handled by poison plus recovery. Turso fsync bugs remain WeftDB bugs.
2. **Poison turns one EIO into "writes down until restart".** This is chosen deliberately over fsyncgate. `/ready` and metrics make it visible, and exit mode is available for supervised deployments.
3. **fsync cost on btrfs/HDD** (ROADMAP.md:925-936). Small single-stream ingest becomes fsync-bound, and the per-aspect commit cap applies. Mitigations: batching guidance, relaxed mode (S17), and the deferred WAL front end. Defaults are decided only after S10/S21 numbers on real volumes.
4. **Hardware or virtual disks that ignore flush break strict mode.** Windows is process-crash-only. NFS is unsupported.
5. **Downgrade hazard.** An old binary on a migrated store issues `MAX(id)+1` ids and rewrites in place. It can still read new frames, because the path column stays absolute, but writing through it is unsafe. Release notes plus a `layout_version` check address this.
6. **Space amplification** comes from up to 2x per component until the reaper runs, from frames pinned by long readers, from backup links and from quarantine. Gauges: retired bytes, quarantine bytes, link-pinned bytes. TTL purge applies to quarantine.
7. **Hard-link backups do not protect against media loss.** Operators must put the backup directory on another device or replicate it.
8. **A bug in the reclaim pins could unlink a frame under a reader.** The live-row guard plus a NotFound re-prune-once fallback in readers limit the blast radius.
9. **Adoption ambiguity.** A post-backup orphan might be a never-committed crash write, which is consistent with the 503/no-response contract (retry with the key).
10. **Legacy Decimal128-to-BigDecimalText widening** changes per-frame encoding mid-aspect. Frames self-describe, but downsample sidecars and bytes/point shift.
11. **Rows-mode residuals** (partial prefix, retry duplicates) remain until aspects move to segments mode.
12. **Reader binding checks are skipped** for legacy rows whose frame_crc is NULL, until backup or scrub fills it in.
13. **Ledger TTL.** A retry after the TTL duplicates rows; reconcile_overlaps removes exact duplicates.

---

## 16. Judge findings and how each is resolved

- **Generation names reused after an aborted swap, and GC replay unlinking live data (Atomic Publish).** Fixed: gens come from a per-aspect counter that is persisted and never reused, and the reaper/R6 skip any name a live row references.
- **Suffix newer-wins broken (Atomic Publish), and a similar hole in "max member id" (WOF/SCM) with allocate-before-write ids.** Fixed by assigning ids inside the commit transaction under aspect.commit. Maintenance inherits min/max member ids.
- **Idempotency ledger not reconciled with quarantine (all three).** Fixed: QuarantineRow runs LedgerInvalidateSpan in the same transaction, over ledger rows whose [min_ts, max_ts] overlaps the quarantined span.
- **Time-based GC grace.** Replaced by reclaim pins and gc_hold.
- **Legacy windows open by default.** Rows mode gets S1/S18 mitigations. Seal-backed mode closes the rest; its default is a user decision.
- **Turso -log recreation after checkpoint.** Refuted for MVCC: the log is truncated in place (logical_log.rs:1032-1093).
- **No fail-stop (WOF/SCM).** Added as write poison, with configurable exit.
- **Buffered-mode detection via LOCK (WOF/SCM).** Replaced by the synced_epoch watermark in the FULL DB.
- **No adoption tool (WOF/SCM).** Restore mode plus the adopt endpoint (S16).
- **split_segment suffix outranking newer seals (WOF/SCM).** The suffix id is assigned in the transaction only when no higher-id row overlaps.
- **Write-ahead enqueue plus hash dedupe can leave two batches for one window (WOF/SCM).** Accepted for rows mode; the damage is limited to derived state. Seal-backed aspects use the change log.
- **Post-ack rollup fold holding the per-aspect lock (WOF/SCM).** The fold is inside the seal transaction.
- **Full CRC pass after an unclean buffered session (WOF/SCM).** Bounded by the watermark.
- **Single store-wide publisher; backup barrier; exit on EIO; no small-write path; encode on async workers (judge 2 on Atomic Publish).** Per-aspect locks plus Turso group commit; no barrier; poison by default; WAL front end deferred; spawn_blocking.
- **Slice sizes and scope (judge 3).** Every slice is 8 h or less, with milestones.
- **Reader pins touching every read site.** Accepted: the same sites change in S6 for path resolution anyway.
- **Legacy bridge under-scoped (judge 3).** Split into S18/S19/S20. No union reads; storage_mode is fixed at creation.
- **LOCK breaks multi-process (judge 3).** Not a regression: Turso already locks DB files exclusively (io/unix.rs:67-71, 278-299).
- **Random nonces hurt recovery and downgrade (judge 3).** Deterministic gen names are used, and absolute paths are kept in the path column.
- **Aspect-name validation could reject existing names (judge 3).** No validation is added; names are encoded instead.
- **SNW-specific flaws** (WAL replay wedges, async-suffix claim, overlay read risk, 2x write amplification, nullable segment_id). Not applicable: there is no WAL in 1.0, and segment_id stays in every 201.
