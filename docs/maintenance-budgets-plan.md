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

**Disk guard (G2, shipped as [#761](https://github.com/basic-automation/weftdb/pull/761))** — `swap_plans` (`seal.rs:348`)
calls `disk_guard.admit_write(total_output_bytes)` at line 399, so a maintenance swap whose
output would take the filesystem past the configured reserve is refused with
`InsufficientStorage` before any frame is written. The admission covers all planned swaps in
one call (`swap_in` is step M5 of `swap_plans`, not a separate writer).

**No memory budget** — no env var or configuration field limits the working-set of a
maintenance merge (decoded frames in memory). The merge reads every member's full frame
before folding (`read_bound_frame` → `merge_members` → `fold_newest_wins`).

**No per-operation timeout budget** — `MaintenanceWait::Skip` (the only variant the daemon
sweeps use) simply skips a busy aspect; `MaintenanceWait::Deadline(duration)` exists but is
never passed by the daemon. There is no per-aspect time-cap on how long a merge/split/compact
may run.

**Settings registry (G1/G4 landed, G6 partly updated)** — `settings.rs:125-138` now registers
`WEFT_MAINTENANCE_MAX_OUTPUT_BYTES` (G1, [#765](https://github.com/basic-automation/weftdb/pull/765)) and
`WEFT_MAINTENANCE_MAX_SEGMENT_BYTES` (G4, [#773](https://github.com/basic-automation/weftdb/pull/773)),
both at `Tier::Experimental`. The tier-list test at `settings.rs:576` asserts they are experimental.
`WEFT_MAINTENANCE_MAX_OP_TIMEOUT_SECS` does not exist — G5 is still open.

**Enforcement summary (per criterion)**:

| Criterion | Status | Evidence |
|---|---|---|
| Byte budget (max bytes per merge output) | **Shipped** [#765](https://github.com/basic-automation/weftdb/pull/765) | `WEFT_MAINTENANCE_MAX_OUTPUT_BYTES` at `settings.rs:137`; `StoreError::OverBudget` at `segment_store.rs:484`; `admit_write` refusal at `seal.rs:399`; HTTP `503 over_budget` at `error.rs:1014` |
| Time budget (max wall-clock per maintenance op) | **Missing** — `MaintenanceWait` deadline exists but daemon uses `Skip` | `reconcile_daemon.rs:426-430` daemon passes `MaintenanceWait::Skip`; no per-op timeout in `MaintGuard`; no `WEFT_MAINTENANCE_MAX_OP_*` setting |
| Memory budget (max decoded rows/frames in flight) | **Missing** — `merge_members` loads all members' full frames | `read.rs:40-45` `merge_members` reads every member; no chunking/gating |
| Disk-guard coverage for maintenance writes | **Shipped** [#761](https://github.com/basic-automation/weftdb/pull/761) | `swap_plans` (`seal.rs:348`) calls `admit_write` at `:399`; test `a_maintenance_swap_refuses_below_the_reserve` guards it |

### 1.2 k-way merge

**Shipped: [#772](https://github.com/basic-automation/weftdb/pull/772) (merged at `52c3a702`).**

The merge primitive `merge_k_way` at
[`split.rs:192-242`](../weft-physical-type/src/split.rs#L192) is a
k-way cursor over a min-heap keyed by `(timestamp, frame_index)`, handling new-wins ordering at
shared timestamps. The pairwise `merge_newer_wins` remains as the two-element case but all
three call sites (`reconcile_overlaps_held`, `squash_series_held`,
`squash_aspect_to_target_rows_held`) now go through `fold_newest_wins` which calls
`merge_k_way` at [`segment_store.rs:1236`](../weftdb/src/types/segment_store.rs#L1236).

A merge of `n` members now runs `O(rows · log n)` comparisons instead of `O(n·rows)`.
The output is provably identical to the old serial two-way fold (verified by benchmark-test
`merge_k_way_matches_the_pairwise_fold_on_every_overlap_shape` at `weft-physical-type/src/split.rs:288`; the same equivalence is re-checked on random shapes in `weft-physical-type/benches/kway_merge.rs`).

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
| G1 | Maintenance byte budget / max output size | **Shipped** [#765](https://github.com/basic-automation/weftdb/pull/765) | `WEFT_MAINTENANCE_MAX_OUTPUT_BYTES` at `settings.rs:137`; `StoreError::OverBudget` at `segment_store.rs:484`; `admit_write` refusal at `seal.rs:399`; HTTP `503 over_budget` with `details.budget_bytes`/`details.planned_bytes` at `error.rs:1014` | — (shipped) | `weft-server` + `weftdb` |
| G2 | Disk guard coverage for maintenance writes | **Shipped** [#761](https://github.com/basic-automation/weftdb/pull/761) | `swap_plans` (`seal.rs:348`) calls `admit_write(total_output_bytes)` at `:399`; test `a_maintenance_swap_refuses_below_the_reserve` guards it | — (shipped) | `weftdb` |
| G3 | k-way / n-way merge (balanced tournament) | **Shipped** [#772](https://github.com/basic-automation/weftdb/pull/772) (merged at `52c3a702`) | `merge_k_way` at `split.rs:198`; used in `fold_newest_wins` at `segment_store.rs:1236`; benchmark `kway_merge` verifies output matches pairwise fold | — (shipped) | `weft-physical-type` + `weftdb` |
| G4 | Automatic resplit of oversized frames | **Shipped** [#773](https://github.com/basic-automation/weftdb/pull/773) | `size_driven_cuts` at `maint.rs:2078` called from `reconcile_overlaps_held` (`:1298`), `squash_series_held` (`:1545`), `squash_aspect_to_target_rows_held` (`:1760`); `WEFT_MAINTENANCE_MAX_SEGMENT_BYTES` at `settings.rs:138` | — (shipped, but single-segment auto-split remains operator-only) | `weftdb` + `weft-server` |
| G5 | Per-op time budget | **Missing** — `MaintenanceWait::Deadline` exists but daemon uses `Skip`; `MaintGuard` has no deadline enforcement | No `WEFT_MAINTENANCE_MAX_OP_*` key; no deadline-enforcement call in sweeps | Wire `MaintenanceWait::Deadline` through the daemon sweeps; add cancellation after timeout to held operations | `weftdb` + `weft-server` |
| G6 | Settings surface for resource budgets | **Partly landed** — `WEFT_MAINTENANCE_MAX_OUTPUT_BYTES` and `WEFT_MAINTENANCE_MAX_SEGMENT_BYTES` exist at `settings.rs:137-138` (Tier::Experimental); `max_op_timeout_secs` still missing | Settings registry lists two `WEFT_MAINTENANCE_MAX_*` keys; tier-list test at `settings.rs:576` asserts them as experimental | Add `max_op_timeout_secs` when G5 is tackled | `weft-server` |

---

## 3. Plan / ordered slices

### ~~Slice 1 — Disk guard coverage for maintenance writes (G2)~~ **SHIPPED** [#761](https://github.com/basic-automation/weftdb/pull/761)

`swap_plans` (`seal.rs:348`) now calls `disk_guard.admit_write(total_output_bytes)` at `:399`.
Test `a_maintenance_swap_refuses_below_the_reserve` guards it.

**On-disk format**: no change.

### ~~Slice 2 — Maintenance byte budget (G1)~~ **SHIPPED** [#765](https://github.com/basic-automation/weftdb/pull/765)

`WEFT_MAINTENANCE_MAX_OUTPUT_BYTES` at `settings.rs:137`, `StoreError::OverBudget` refusal at
`segment_store.rs:484`, `admit_write` at `seal.rs:399`, HTTP `503 over_budget` at `error.rs:1014`.

**On-disk format**: no change.

### ~~Slice 3 — k-way merge (G3)~~ **SHIPPED** [#772](https://github.com/basic-automation/weftdb/pull/772)

`merge_k_way` at `split.rs:192-242`; `fold_newest_wins` calls it at `segment_store.rs:1236`.
Benchmark `kway_merge` verifies output matches pairwise fold.

**On-disk format**: no change (output is the same rows).

### ~~Slice 4 — Automatic resplit of oversized frames (G4)~~ **SHIPPED** [#773](https://github.com/basic-automation/weftdb/pull/773)

`size_driven_cuts` at `maint.rs:2078` called from all three fold sites (`:1298`, `:1545`, `:1760`).
`WEFT_MAINTENANCE_MAX_SEGMENT_BYTES` at `settings.rs:138`.

**Landed differently, deliberately.** The cut that landed re-cuts the output of
`reconcile_overlaps_held`, `squash_series_held` and `squash_aspect_to_target_rows_held` inside
the `SwapPlan::cut` that already writes it, and caps the outputs at the number of segments the
fold replaced: one output per member means every output takes a member's id (precedence P1) and
none needs a fresh id, so the refusal that rule P2 documents cannot fire at all. The unwritten
test above asks for the one case that cannot be done that way — a **single** giant segment, whose
second output would need a fresh id and would be refused if any higher-id row overlapped its
span. That case remains the operator's `split_segment`, and the plan's own risk note was exactly
why it was left there.

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
they are **three independent axes**:

| Wording | Status now (post-ROB-20) |
|---|---|
| Resource limits | **Partly shipped.** Disk-guard coverage (G2, #761) and byte budget (G1, #765) are on main.
  Per-op time budget (G5) and memory budget remain missing. |
| k-way merge | **Shipped** [#772](https://github.com/basic-automation/weftdb/pull/772) — `merge_k_way` at `split.rs:198`. |
| Re-splitting of oversized frames | **Shipped** [#773](https://github.com/basic-automation/weftdb/pull/773) — auto-recut of fold outputs
  under `WEFT_MAINTENANCE_MAX_SEGMENT_BYTES`. A single-segment auto-split remains operator-only. |

The roadmap line at `ROADMAP.md:85` reflects the same grouping and cannot be ticked until
G5 and the memory budget are closed, or the wording is revised to match the narrower scope.