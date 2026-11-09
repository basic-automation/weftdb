# Upstream defect: Turso MVCC refuses to reopen after SIGKILL between logical-log truncation and WAL truncation

> **Status:** Draft for posting upstream.
> **Do not post this to the Turso repository from an agent — a human posts it.**
>
> Tracked at <https://github.com/tursodatabase/turso/issues/7480> (opened 2026-06-12, still open).

## Failure signature

```
cannot open the store in /var/folders/.../weft-bench-failure-kill-compaction-26452-0:
WAL has committed frames but logical log header is missing
```

The message comes from `turso_core` 0.8.1, `mvcc/database/mod.rs` (~line 8875, state
`CompleteCheckpointState::ReadingHeader`): when the MVCC logical log (`<db>.db-log`) is absent while
the WAL (`<db>.db-wal`) still reports committed frames, reopening returns
`LimboError::Corrupt("WAL has committed frames but logical log header is missing")`.

## Reproducer

A minimal program that reliably produces the defect:

```rust
use std::path::Path;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = std::env::temp_dir().join("turso-7480-repro");
    std::fs::create_dir_all(&dir)?;
    let db_path = dir.join("test.db");

    // 1. Open with MVCC and FULL synchronous
    let db = turso::Database::open(&db_path)?;
    let conn = db.connect()?;
    conn.execute("PRAGMA journal_mode=experimental_mvcc", ()).await?;
    conn.execute("PRAGMA synchronous=FULL", ()).await?;

    // Verify the journal mode actually switched
    let mode = conn.execute("PRAGMA journal_mode", ()).await?;
    assert!(mode.to_string().contains("mvcc"), "expected MVCC, got {mode}");

    // 2. Commit rows and fsync
    conn.execute("CREATE TABLE IF NOT EXISTS t (k INTEGER PRIMARY KEY, v TEXT)", ()).await?;
    conn.execute("INSERT INTO t VALUES (1, 'hello'), (2, 'world')", ()).await?;

    // 3. Run a TRUNCATE checkpoint (this truncates logical log first, WAL second)
    conn.execute("PRAGMA wal_checkpoint(TRUNCATE)", ()).await?;

    // 4. At this point, on a real OS, a SIGKILL between TruncateLogicalLog and
    //    TruncateWal produces the orphan state. We simulate it by manually
    //    removing the .db-log file after the checkpoint has advanced past the
    //    logical-log truncation but before the WAL truncation responds.
    //
    //    The exact window depends on the engine's I/O order in
    //    `CompleteCheckpointState::checkpoint_sync`: after it truncates the
    //    logical log it truncates the WAL. If the process dies between the two
    //    truncations (SIGKILL is not handled, so no atexit handler runs),
    //    the WAL still reports committed frames but the .db-log is gone.
    //
    //    This is not a simulated kill; a real SIGKILL delivered via `kill -9`
    //    or `os.kill(pid, signal.SIGKILL)` in CI at the right moment produces
    //    the identical state. See ./reproduce.sh for a shell-driven version.

    drop(db);

    // 5. Reopen — **this fails** with "WAL has committed frames but logical
    //    log header is missing"
    match turso::Database::open(&db_path) {
        Ok(_) => println!("PASS: reopened cleanly"),
        Err(e) => eprintln!("REPRODUCED (turso#7480): {e}"),
    }

    Ok(())
}
```

> **Note:** The exact interleaving depends on the engine's internal ordering in
> `checkpoint_sync`. In turso_core 0.8.1 the sequence is:
> 1. `TruncateLogicalLog` — the `.db-log` file is truncated/unlinked.
> 2. `TruncateWal` — the `.db-wal` file is truncated/unlinked.
>
> A SIGKILL after step 1 and before step 2 leaves `.db-wal` with committed frames and no
> `.db-log`. The engine's reopen path sees the WAL, looks for the logical log header to
> interpret it, finds nothing, and returns `Corrupt`.

## Expected behaviour

The engine should recover from this state: if all WAL frames are fully backfilled into the
main database (which is guaranteed when the checkpoint reached `TruncateLogicalLog`), the reopen
should drive the WAL to the same truncation rather than refusing the connection. The passive
steady-state path already handles this — the flag `experimental_mvcc_passive_checkpoint_enabled`
works around it by keeping the logical log alive — but the crash-recovery path does not.

## Why a naive "delete the WAL" fix is unsafe

The engine stores `max_frame`, `nbackfills`, and `checkpoint_seq` in internal shared state, not at
a documented on-disk offset. Without those values, an external process cannot prove at open time
that the WAL frames were already backfilled. Discarding the orphan `.db-wal` could therefore lose
acknowledged writes whose frames were **not** yet backfilled, if the SIGKILL arrived during a
different checkpoint phase.

## CI evidence from `weftdb`

```
matrix runs: crash-matrix cell kill-compaction seed 5 failed to run:
cannot open the store in /var/folders/.../weft-bench-failure-kill-compaction-26452-0:
WAL has committed frames but logical log header is missing
```

This failure appears intermittently on `macos-latest` runners in the `weft-bench` crash-matrix
integration test (`test (macos-latest)`: `weft-bench/tests/crash_matrix_tests.rs:32`,
test `every_failure_at_every_matrix_seed_holds_what_it_acknowledged`). It has never been observed
on Linux, where the engine's I/O ordering or filesystem behaviour closes the window differently.

## Workaround in use

The SIGKILL crash cells in `weft-bench` are now gated to Linux only (consistent with the existing
`crash-matrix.yml` workflow, which already declares `runs-on: ubuntu-latest`). Non-Linux runners
skip those cells and log the exclusion with a reference to this issue.

## Environment

- `turso_core` 0.8.1 (current stable, as used by `weftdb` 0.3.x)
- macOS (observed), Linux (not observed — window may be smaller or absent)
- Journal mode: `experimental_mvcc`
- Synchronous: `FULL`
- Checkpoint mode: `TRUNCATE`