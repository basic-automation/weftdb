# Panic audit on server paths

Audit ID: **P242-2026-11-14**  
Scope: All production `.rs` files in `weft-server/src/` and `weftdb/src/` reachable from server request handlers (ingest, query, control-plane write).  
Method: Static grep on every panic-capable construct (`unwrap()`, `expect()`, `panic!()`, `unreachable!()`, `todo!()`, `assert!()`, fallible array indexing, `Mutex`/`RwLock` poisoning, `SystemTime` arithmetic). Each site is tagged with **reachability** and a **verdict**.

## Verdict labels

| Label | Meaning |
|-------|---------|
| **OK, safe** | Reachable from a catch boundary but verified safe by construction (invariant, literal, hash size, etc.) |
| **OK, test-only** | Behind `#[cfg(test)]`, `#[tokio::test]`, or a `mod tests` block |
| **OK, unreachable from catch** | Dead code, control-plane-only (aborted by ROB-2), or daemon-only without a panic catch |
| **NEEDS-FIX** | Reachable from a catch boundary and a user payload could realistically trigger the panic |

## Catch boundary

All request handling (ingest, query) enters through the middleware stack in `lib.rs`:

```
panic_catch::catch_panic (middleware)
```

- **Non-ingest handler** panics → `ApiError::internal` (500 error response).  
- **Ingest handler** panics → `map_ingest_panic` per-phase retry or 400/500.  
- **Control-plane writes** (backup, restore, prune) are aborted by the ROB-2 hook **before** the catch layer, so a panic in a control-plane daemon bypasses the request middleware but does not crash the process (the daemon task terminates and the daemon supervisor restarts it).

The audit marks a site **NEEDS-FIX** if it is reachable from an HTTP handler path and can be triggered by a malformed or adversarial payload. A site is **OK, safe** if it is reachable but guarded by an invariant that cannot be violated by any payload.

---

## Sites by file

### `weft-server/src/serve.rs`

| Line | Construct | Reachability | Verdict | Notes |
|------|-----------|--------------|---------|-------|
| 423 | `permit.expect("the semaphore is never closed")` | Request-path (accept loop) | **OK, safe** | `Semaphore::acquire_owned` returns `Err(Closed)` only when the semaphore is closed; the accept-loop semaphore is never closed. |

### `weft-server/src/storage.rs`

| Line | Construct | Reachability | Verdict | Notes |
|------|-----------|--------------|---------|-------|
| 483 | `hash[..8].try_into().unwrap()` | Query-path (`hash_selector`) | **OK, safe** | SHA-256 output is 32 bytes; slicing `[..8]` always fits a `[u8; 8]`. |
| 498 | `hash[..8].try_into().unwrap()` | Query-path (`fingerprint_rows`) | **OK, safe** | Same invariant. |

### `weft-server/src/interpolate.rs`

| Line | Construct | Reachability | Verdict | Notes |
|------|-----------|--------------|---------|-------|
| 152 | `NonZeroUsize::new(MAX_INTERPOLATE_OUTPUT_POINTS).expect(...)` | Module init (constant) | **OK, safe** | `MAX_INTERPOLATE_OUTPUT_POINTS = 10_000_000` is a literal > 0. |

### `weftdb/src/types/batches/batch.rs`

| Line | Construct | Reachability | Verdict | Notes |
|------|-----------|--------------|---------|-------|
| 319 | `self.measurements[0].clone()` (index `0`) | Ingestion path via `process_batches` → `simplify_transformation` | **OK, safe** | `is_empty()` and `len() <= 2` early returns guarantee `len() >= 3` at this point. |
| 341 | `self.measurements.last().unwrap().clone()` | Ingestion path via `simplify_transformation` | **OK, safe** | First measurement was already pushed into `kept_measurements`; `kept_measurements` is never empty here. |

### `weftdb/src/types/compression/algorithm.rs`

| Line | Construct | Reachability | Verdict | Notes |
|------|-----------|--------------|---------|-------|
| 59 | `measurements.last().unwrap()` | Query/analysis path via `aspect.rs:2149` → `simplify_with_aggressiveness` | **OK, safe** | Function returns empty `Vec` if empty input, and returns early if `<= 2` measurements; `.unwrap()` is reached only when `len() > 2`. |
| 71 | `measurements.last().unwrap()` | Same path | **OK, safe** | Same guard. |
| 147 | `points.last().unwrap()` | Same path (via `simplify_by_slope_change`) | **OK, safe** | `points.len() <= 2` early return guarantees `len() >= 3`. |

### `weftdb/src/types/database/outputs.rs`

| Line | Construct | Reachability | Verdict | Notes |
|------|-----------|--------------|---------|-------|
| 402 | `all_measurements.last().unwrap().timestamp()` | Query/analysis path via `analyze_point` | **OK, safe** | `all_measurements` is populated from paginated DB queries. The `is_empty()` guard at line 376 (`if all_measurements.is_empty() { bail!(…) }`) makes the vector non-empty by line 402, so `first().unwrap()`/`last().unwrap()` cannot fire. The guard is not itself a panic: `bail!` is `anyhow::bail`, which expands to `return Err(anyhow!(…))` (see the *Cleared* section), and `analyze_point` returns `anyhow::Result<Point>`. |

### `weftdb/src/types/aspect.rs`

| Line | Construct | Reachability | Verdict | Notes |
|------|-----------|--------------|---------|-------|
| 431 | `database_metadata().unwrap()` | **OK, unreachable from catch** | Only called in the `#[cfg(test)]` block at the end of `aspect.rs`. |

### `weftdb/src/types/batches/batch.rs` — `relative_analysis`

| Line | Construct | Reachability | Verdict | Notes |
|------|-----------|--------------|---------|-------|
| 366 | `locations.iter().min().ok_or_else(...)?.clone()` (no `.unwrap()`) | N/A — uses `ok_or_else` + `?` | Not a panic site. |
| 370 | `locations.iter().max().ok_or_else(...)?.clone()` | Same — safe error propagation | Not a panic site. |
| 423 | `self.measurements.last().and_then(...)` | Batch analysis path | Uses safe Option chaining, no panic. |

### Daemon files (no request-path catch boundary, but task-restart boundary)

These are in `weft-server/src/` but are daemon-only paths. A panic in a daemon kills that async task; the daemon supervisor (`daemons.rs`) restarts it.

| File | Line | Construct | Verdict | Notes |
|------|------|-----------|---------|-------|
| `reconcile_daemon.rs:647,672` | `.lock().expect("log buffer")` | **OK, safe** (daemon test helpers only, behind `#[cfg(test)]` at line 444) |
| `storage.rs:2234,2276` | `.lock().expect("log buffer")` | **OK, safe** (test helpers only, behind `#[cfg(test)]` at line 1351) |
| `backup_daemon.rs:531` | `.duration_since(...).unwrap()` | **OK, safe** (test code only, inside `#[tokio::test]`) |
| `backup_daemon.rs:787,790` | `assert!` / `.unwrap()` | **OK, safe** (test code only) |
| `audit.rs:551-552` | `.expect()` on File open/read | **OK, safe** (test code only, behind `#[cfg(test)]`) |
| `auth.rs:860` | `.expect("the test keys file parses")` | **OK, safe** (test code only) |

### All `panic!()`, `unreachable!()`, `todo!()`

| File | Line | Construct | Verdict | Notes |
|------|------|-----------|---------|-------|
| `weftdb/src/types/durable/dirsync.rs:233..286` | `unreachable!()` | **OK, safe** | In `ProbeFs` which is `#[cfg(test)]` only (a probe/dummy filesystem). |
| All other `panic!()` / `unreachable!()` calls in `weft-server/` and `weftdb/` | — | **OK, safe** | Verified to be behind `#[cfg(test)]` or inside `match` arms that match exhaustively. |

### Mutex / RwLock poisoning

All lock access in production code uses:
- `.unwrap_or_else(PoisonError::into_inner)` — the `PoisonError::into_inner` handler converts a poisoned lock to an `Err`; no panic.
- `.lock().expect("log buffer")` — only used in test helpers.

No bare `.lock().unwrap()` or `.lock().unwrap_or_else(|_| panic!(...))` exists in production code.

### `SystemTime` arithmetic

All production uses of `SystemTime::now().duration_since(...)` use `map_or(0, ...)`, `?`, or `map_err(...)?.` — never bare `.unwrap()`. Safe.

### `assert!()` in production code

No production-path `assert!()` found outside `#[cfg(test)]` blocks.

---

## Summary

| Verdict | Count |
|---------|-------|
| **OK, safe** | 13 |
| **OK, unreachable from catch** | 1 (`aspect.rs:431`) |
| **OK, test-only** | ~18 |
| **NEEDS-FIX** | 0 |

---

## Cleared during review

### `weftdb/src/types/database/outputs.rs:376` — `bail!("No measurements found for aspect")`

**Location:** `analyze_point`, which returns `anyhow::Result<Point>` (`use anyhow::{bail, Result};` at `outputs.rs:3`).

An earlier revision of this audit filed this as NEEDS-FIX on the ground that `bail!`
"expands to `panic!()`". **That is wrong**, and the correction matters: a panic here
would be a real server defect, so the claim was checked against the macro source.
`anyhow::bail` expands to a controlled error return — `anyhow-1.0.104/src/macros.rs:58`:

```rust
macro_rules! bail {
    ($msg:literal $(,)?) => {
        return $crate::__private::Err($crate::__anyhow!($msg))
    };
```

So an empty result set propagates an `Err` to the caller, which renders it as an ordinary
API error. There is no panic, nothing to fix, and the site is **OK, safe**.

### `weftdb/src/types/database/outputs.rs:402` — `all_measurements.last().unwrap().timestamp()`

**Location:** Same `analyze_point` method, inside the same function.  
**Trigger:** Unreachable on an empty vector: the guard at line 376 returns an `Err` in that
case (see above), so by line 402 `all_measurements` is non-empty and both `first().unwrap()`
and `last().unwrap()` hold. **OK, safe.**

**Follow-up:** issue #748, opened from the mistaken premise above, was closed as not-a-bug.

---

## Daemon panic resilience

Each daemon (backup, reconcile, maintain) that runs as an async task is supervised by `daemons.rs`. A panic inside a daemon task kills that task, but the supervisor detects termination, logs it, and restarts the task. This means:

- A panic in a daemon does **not** crash the process.
- The catch boundary for daemons is **task-level**, not middleware-level.
- The audit verdict for daemon-only sites is **OK, safe** when the panic would restart the task without data loss.

**Exception:** A panic inside a critical section (e.g., halfway through a control-plane write) could leave partial state. ROB-2 (crash-consistency) requires that these writes are idempotent or atomic, so a restart re-tries the operation. This is validated by crash-consistency tests, not by this audit.