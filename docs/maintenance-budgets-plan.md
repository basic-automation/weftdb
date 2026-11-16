# Maintenance Budgets, k-Way Merge and Re-Splitting — Audit and Plan

**Issue #445** — docs-only audit of what the tree does today, gaps, and ordered slices.

## 1. What the tree does today

### 1.1 Maintenance resource limits / budgets

There is **no per-maintenance-operation resource budget** (byte, time, or row-count ceiling
enforced before a merge/split/compact starts). The tree relies on two disjoint mechanisms,
neither of which is a maintenance budget in the conventional sense:

| Mechanism | File(s) | Enforced against | Default |
|---|---|---|---|
| Disk guard (ROB-17) | [`seal.rs:1093-1106`](../weftdb/src/types/segment_store/seal.rs#L1093) — `admit_write` | Only **ingest/seal writes**, **not** maintenance swap outputs | `disk_reserve_bytes: 0` = off (library); `WEFT_DISK_RESERVE_BYTES` env (server) |
| Maintenance breaker (ROB-19) | [`segment_store.rs:696-738`](../weftdb/src/types/segment_store.rs#L696) — `MaintenanceBreaker`; [`settings.rs:147-148`](../weft-server/src/settings.rs#L147) — registry entry | Consecutive **sweep failures** per aspect, not resource consumption | `failure_threshold=3`, `cooldown=300s` (`WEFT_MAINTENANCE_BREAKER_FAILURES` / `WEFT_MAINTENANCE_BREAKER_COOLDOWN_SECS`) |

**Disk guard exclusion** — `swap_plans` (`seal.rs:348`) and `swap_in` (the low-level writer)
never call `disk_guard.admit_write`. Grep pattern `admit_write` searched across the entire
`segment_store/` directory: the only call site is `seal.rs:1096`, inside `seal_declared` and its
callers (`seal_declared_many`, `seal_imported`). A maintenance swap that enlarges the store
while free space is below the reserve will succeed and risk ENOSPC.

**No memory budget** — no env var or configuration field limits the working-set of a
maintenance merge (decoded frames in memory). The merge reads every member's full frame
before folding (`read_bound_frame` → `merge_members` → `fold_newest_wins`).

**No per-operation timeout budget** — `MaintenanceWait::Skip` (the only variant the daemon
sweeps use) simply skips a busy aspect; `MaintenanceWait::Deadline(duration)` exists but is
never passed by the daemon. There is no per-aspect time-cap on how long a merge/split/compact
may run.

**Settings registry** (`settings.rs:140-152`) registers `WEFT_MAINTENANCE_BREAKER_COOLDOWN_SECS`
and `WEFT_MAINTENANCE_BREAKER_FAILURES`. No `WEFT_MAINTENANCE_*` key controls a byte, row, or
time budget for the operation itself.

**Enforcement summary (per criterion)**:

| Criterion | Status | Evidence |
|---|---|---|
| Byte budget (max bytes per merge output) | **Missing** — no env var or struct field; swap output writes what the merge produces | `seal.rs:348` `swap_plans`; no `admit_write` call; search `max_bytes`, `budget`, `quota` across `segment_store/` (zero maintenance hits) |
| Time budget (max wall-clock per maintenance op) | **Missing** — `MaintenanceWait` deadline exists but daemon uses `Skip` | `reconcile_daemon.rs:426-430` daemon passes `MaintenanceWait::Skip`; no per-op timeout in `MaintGuard` |
| Memory budget (max decoded rows/frames in flight) | **Missing** — `merge_members` loads all members' full frames | `read.rs:40-45` `merge_members` reads every member; no chunking/gating |
| Disk-guard coverage for maintenance writes | **Missing** — only ingress/seal writes are guarded | `seal.rs:1093` vs no call in `swap_plans` or `swap_in` |

### 1.2 k-way merge

**Shipped: pairwise (two-way) serial fold, not n-way.**

The merge primitive, `merge_newer_wins` at
[`split.rs:147-177`](../weft-physical-type/src/split.rs#L147), is a classic two-pointer
merge of two sorted runs:

```rust
pub fn merge_newer_wins<V: Clone>(older: &[(i64, V)], newer: &[(i64, V)]) -> Vec<(i64, V)> {
    // two-pointer merge, O(len(older) + len(newer))
}
```

The caller, `fold_newest_wins` at
[`segment_store.rs:1065-1073`](../weftdb/src/types/segment_store.rs#L1065),
calls it **iteratively** — not as a balanced tree or priority queue:

```rust
fn fold_newest_wins(frames: Vec<DecodedFrame>) -> (Vec<i64>, Vec<Option<BigDecimal>>) {
    let mut merged = Vec::new();
    for frame in frames {
        let rows = /* sort by timestamp */;
        merged = merge_newer_wins(&merged, &rows);
    }
    merged.into_iter().unzip()
}
```

Every call path that merges more than two members uses this same serial fold:

- **Overlap merge** — `reconcile_overlaps_held` (`maint.rs:1080`): groups overlapping
  segments into components (≥ 2), sorts by id, and calls `merge_members_against_first`
  which internally calls `fold_newest_wins`/`merge_newer_wins`.
- **Squash** — `squash_series_held` (`maint.rs:1340`): folds every segment of a series
  into one via `merge_members_against_first`.
- **Compaction** — `squash_aspect_to_target_rows_held` (`maint.rs:1521`): greedily groups
  consecutive segments and folds each group.

**No k-way / n-way merge exists.** A merge of `n` members runs `n` rounds of two-way
merges, writing the accumulator each time. The accumulator grows monotonically (all timestamps
from earlier members are carried forward), so the last round does `O(n·rows)` work where a
balanced k-way would do `O(rows · log k)` comparisons.

**Fan-in limit**: none. The component or group size determines how many members are folded.
In practice capped by the number of overlapping segments an aspect accumulates before
a sweep.

### 1.3 Re-splitting of oversized frames

**Partial: overlap-merge split exists; as of the G4 cut, a fold re-cuts an oversized output of its own; a frame a single seal wrote is still not re-split automatically.**

**What exists** — `reconcile_overlaps_with_policy` (`maint.rs:1071`) and its held variant:
when an overlap component's cold prefix is large enough relative to the hot suffix,
`SplitPolicy::decide` may choose `SplitDecision::Split` and the cold prefix is carved off
as a separate output segment (`maint.rs:1214-1215`). This is the Phase 4.6 split-not-rewrite
path. The trigger is cross-segment **time overlap**, not frame size.

**What does not exist** — any automatic mechanism that detects a single segment whose on-disk
size (or row count) exceeds a threshold and splits it into smaller frames. The only split
operation, `split_segment` (`maint.rs:634`), is an **operator-triggered** API that requires
an explicit `boundary` timestamp:

```rust
pub async fn split_segment(&self, aspect: &str, id: u64, boundary: i64) -> Result<Option<u64>>
```

It is never called by any daemon, sweep, or tick. Grep for `split_segment` call sites across
the whole tree:

```
weft-server/src/routes.rs        — the HTTP route handler (operator API)
weftdb/src/types/segment_store/maint.rs — the definition
```

No automated caller. A segment that grows past an operator's preferred size stays that size
until an operator manually splits it.

**Default output size**: the merge output is not framed to a target byte size. The paging
parameter `MERGE_ROWS_PER_PAGE = 4096` (`segment_store.rs:1083`) controls page layout, not
output size. An aspect's single segment after a full squash holds all rows of every series,
which can be arbitrarily large.

---

## 2. Gap table

| # | Accepted behaviour | Status | Evidence | Smallest closure slice | Crate |
|---|---|---|---|---|---|
| G1 | Maintenance byte budget / max output size | **Missing** | No env var, no `admit_write` call in `swap_plans`; no maintenance-budget field in `SegmentStoreOptions` or daemon config | Add a `max_output_bytes` field to `ReconcileDaemonConfig` and check cumulative swap-plan bytes against it before writing | `weft-server` + `weftdb` |
| G2 | Disk guard coverage for maintenance writes | **Missing** | `swap_plans` (`seal.rs:348`) and `swap_in` have no `disk_guard.admit_write` call | Call `disk_guard.admit_write(total_output_bytes)` before `swap_plans` executes, return `InsufficientStorage` if refused | `weftdb` |
| G3 | k-way / n-way merge (balanced tournament) | **Missing** | `fold_newest_wins` does pairwise serial fold; `merge_newer_wins` is strictly two-pointer | Replace `fold_newest_wins`'s serial loop with a stable k-way merge via binary heap. Moves merge work from O(n·rows) to O(rows · log k) | `weft-physical-type` + `weftdb` |
| G4 | Automatic resplit of oversized frames | **Partly landed** (the G4 cut: the ceiling drives the re-cut of a fold's output inside the same swap) | `split_segment` is still operator-only; no daemon calls it; the threshold now drives the re-cut of merge/squash output only | Add `max_segment_bytes` to daemon config; after a merge/squash/compact, if output exceeds threshold, re-split on a coarse boundary (e.g. timestamp midpoint or row-count midpoint) | `weftdb` + `weft-server` |
| G5 | Per-op time budget | **Partial** | `MaintenanceWait::Deadline` exists but daemon uses `Skip`; `MaintGuard` has no deadline enforcement | Wire `MaintenanceWait::Deadline` through the daemon sweeps; add cancellation after timeout to held operations | `weftdb` + `weft-server` |
| G6 | Settings surface for resource budgets | **Missing** | Settings registry lists only breaker params; no `WEFT_MAINTENANCE_MAX_*` keys | Add `max_output_bytes`, `max_segment_bytes`, `max_op_timeout_secs` vars to `settings.rs` | `weft-server` |

---

## 3. Plan / ordered slices

### Slice 1 — Disk guard coverage for maintenance writes (G2)

**Goal**: prevent maintenance swap outputs from writing past the disk reserve.

**Files**:
- `weftdb/src/types/segment_store/seal.rs` — insert `disk_guard.admit_write(total_bytes)` at
  the top of `swap_plans`, before the transaction loop.
- `weftdb/src/types/segment_store/maint.rs` — compute `total_bytes` from `SwapPlan` members'
  `byte_len`, or from the output frame's estimated size.

**Acceptance test**: a test that fills the store to just above the reserve, then triggers an
overlap merge — before the fix it writes (hits ENOSPC or succeeds dangerously), after it
returns `InsufficientStorage`.

**Risk**: a merge that cannot write its output because of the disk guard will fail the
entire aspect's merge. The per-aspect breaker already handles this (consecutive failure →
skip). Low risk.

**On-disk format**: no change.

### Slice 2 — Maintenance byte budget (G1)

**Goal**: cap the byte size of a single maintenance operation's output.

**Files**:
- `weft-server/src/startup/daemons.rs` — add `max_output_bytes` to `ReconcileDaemonConfig`.
- `weft-server/src/settings.rs` — add `WEFT_MAINTENANCE_MAX_OUTPUT_BYTES` registry entry.
- `weftdb/src/types/segment_store/maint.rs` — thread a `max_output_bytes` parameter through
  the merge path; skip a component/group when its total member bytes would exceed the cap,
  leaving it for a subsequent pass after the daemon re-reads the index.

**Acceptance test**: set a small `WEFT_MAINTENANCE_MAX_OUTPUT_BYTES`, seal data that would
exceed it, verify the overlap merge passes without rewriting the oversized component.

**Risk**: skipping large groups means they stay large until an operator acts. Must be
documented as an advisory cap, not a hard split trigger. Low risk.

**On-disk format**: no change.

### Slice 3 — k-way merge (G3)

**Goal**: replace the serial two-way fold with a stable k-way merge using a binary heap on
`(timestamp, frame_index)`.

**Files**:
- `weft-physical-type/src/split.rs` or new file — add `pub fn merge_k_way<V: Clone>(frames: &[&[(i64, V)]]) -> Vec<(i64, V)>`.
- `weftdb/src/types/segment_store.rs` — replace the loop in `fold_newest_wins` with a call to
  `merge_k_way`.
- Optionally thread `V` generically through frame-decoded types.

**Acceptance test**: property-test that the k-way output matches the serial-fold output for
any set of frames (same timestamps, same value at each timestamp). Benchmark that k-way is
faster for k > 4 on realistic frame sizes.

**Risk**: the existing merge is simple and well-tested. A k-way implementation must handle the
newer-wins semantic (at a shared timestamp, the newest frame's rows supersede). In a heap
this means draining all heap entries that share the minimum timestamp from the newest frame
first, then the older frames. The behaviour at equal timestamps must match `merge_newer_wins`
exactly.

**On-disk format**: no change (output is the same rows).

### Slice 4 — Automatic resplit of oversized frames (G4)

**Goal**: after a merge/squash/compact produces an output frame larger than a threshold,
split it into two (or more) roughly equal-sized frames on a natural boundary.

**Files**:
- `weftdb/src/types/segment_store/maint.rs` — add a `max_segment_bytes` parameter to
  `reconcile_overlaps_held`, `squash_series_held`, `squash_aspect_to_target_rows_held`.
  After creating the `SwapPlan`, check the output byte estimate against the threshold; if
  exceeded, call `split_frame` (or a new helper) to produce two `SwapPlan`s instead of one.
- `weft-server/src/startup/daemons.rs` — thread `max_segment_bytes` from config.
- `weft-server/src/settings.rs` — register `WEFT_MAINTENANCE_MAX_SEGMENT_BYTES`.

**Boundary heuristic**: split at `row_count / 2` (by timestamp). This is coarse but
deterministic. A more sophisticated heuristic (byte-accurate estimate from codec + row count)
can be layered later.

**Acceptance test**: configure a small `max_segment_bytes`, set up an aspect with a single
giant sealed segment, run compaction, assert the aspect now has multiple segments each below
the threshold.

**Landed differently, deliberately.** The cut that landed re-cuts the output of
`reconcile_overlaps_held`, `squash_series_held` and `squash_aspect_to_target_rows_held` inside
the `SwapPlan::cut` that already writes it, and caps the outputs at the number of segments the
fold replaced: one output per member means every output takes a member's id (precedence P1) and
none needs a fresh id, so the refusal that rule P2 documents cannot fire at all. The test above
asks for the one case that cannot be done that way — a **single** giant segment, whose second
output would need a fresh id and would be refused if any higher-id row overlapped its span. That
case remains the operator's `split_segment`, and the plan's own risk note ("must validate that
new-id allocation stays correct under concurrent splits") is exactly why it was left there.

**Risk**: splitting a segment changes the segment count, which moves P1 precedence.
The lowest-id rule (`SwapPlan::cut` with `cuts`) handles this correctly — the split output
lives under the lowest id and a new id. The segment count increases, which the squash pass
is designed to bound. Medium risk — must validate that new-id allocation stays correct under
concurrent splits.

**On-disk format**: no change (segments are already frame-structured).

### Slice 5 — Per-op time budget via daemon (G5)

**Goal**: the daemon's sweeps use `MaintenanceWait::Deadline` with a configurable timeout
so a stuck merge does not block that aspect indefinitely.

**Files**:
- `weft-server/src/startup/daemons.rs` — add `op_timeout_secs` to `ReconcileDaemonConfig`.
- `weft-server/src/reconcile_daemon.rs` — pass the timeout through to the sweep calls
  instead of `MaintenanceWait::Skip`.

**Acceptance test**: set a very short timeout (1 second), seal enough data that the merge
takes longer, verify the daemon skips the aspect as busy on the next tick.

**Risk**: the merge is not actually cancelled — it runs to completion on a background task.
The timeout only controls how long the sweep waits to acquire the aspect lock. Medium risk
if the user expects hard cancellation.

**On-disk format**: no change.

---

## 4. What must not regress

The following tests guard maintenance invariants. Every implementation slice must pass them
unchanged.

### Breaker and cancellation

| Test file | Test | What it guards |
|---|---|---|
| `segment_store/swap_tests.rs` | `swap_refuses_mixed_series` (I12) | A swap plan never mixes two series |
| `segment_store/swap_tests.rs` | `swap_atomic` | Swap is atomic at the aspect level |
| `segment_store/tag_maint_tests.rs` | `swap_plan_refuses_mixed_series` | Same I12 at plan level |
| `segment_store/maint.rs` (impl) | `breaker_permits` / `breaker_record` | Circuit breaker correctly skips and retries |
| `segment_store/maint.rs` | `cancel_requested` | A cancellation token stops the sweep before the next aspect |
| `segment_store/disk_guard_tests.rs` | `the_library_default_leaves_the_guard_off` | Disk guard default is off (no regression) |
| `segment_store/disk_guard_tests.rs` | `reserve_is_enforced_post_seal` | Disk guard blocks seal writes below reserve |

### Overlap merge

| Test file | Test | What it guards |
|---|---|---|
| `segment_store/overlap_read_tests.rs` | `disjoint_segments_concatenate_without_a_conflict` | Non-overlapping segments concatenate |
| `segment_store/overlap_read_tests.rs` | overlapping case tests | Newer-wins at shared timestamp |
| `segment_store/soak/maint_soak_test.rs` (if present) | Merge under concurrent seal | Stability under load |

### Squash and compaction

| Test file | Test | What it guards |
|---|---|---|
| `segment_store/tag_maint_tests.rs` | series-level squash tests | TAG-6: one plan per series |
| `segment_store/maint.rs` (doc-tests / unit tests) | `squash_aspect_to_target_rows_if_fragmented` | Fragmentation gate |

### Split

| Test file | Test | What it guards |
|---|---|---|
| `segment_store/tag_maint_tests.rs` | split against boundary | `split_segment` boundary correctness |
| `weft-physical-type/src/split.rs` | `merge_newer_wins` unit tests | Two-pointer merge correctness |
| `weft-physical-type/src/split.rs` | `split_index` tests | Partition-point correctness |

### Daemon wiring

| Test file | Test | What it guards |
|---|---|---|
| `scripts/ci/test_check_serial_order.py` | serial test ordering | Linter for test attribute ordering |
| Integration tests (if contract tests exist) | FRE-10a contract | Probe/compute/roles |

---

## 5. Disagreements with 1.0-plan.md wording

The 1.0-plan's Phase 6 W9 wording *"Maintenance resource limits, k-way merge and re-splitting
of oversized frames"* groups three behaviours as if they are a single lane. The audit shows
they are **three independent axes**, each with a different shipping status:

| Wording | Status today |
|---|---|
| Resource limits | Most mechanisms missing (disk guard excludes maint; no byte/time/memory budgets) |
| k-way merge | Not shipped; pairwise serial fold only |
| Re-splitting of oversized frames | Only the overlap-split path (Phase 4.6) exists; automatic size-driven resplit is missing |

The roadmap line at `ROADMAP.md:85` reflects the same grouping and cannot be ticked until
all three are shipped or the wording is revised.