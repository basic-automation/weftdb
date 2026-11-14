# Progress — TEST-658

## Handoff — 2026-11-10

### Changed files

- `weftdb/src/types/segment_store/overlap_read_tests.rs` (139 insertions, 6 deletions)

### Commit

- Commit `e350741` on `test/overlap-two-segments`, subject `[TEST] Make the overlap read tests seal two segments (#658)`, signed-off.
- Pushed to `origin/test/overlap-two-segments`.
- PR opened at https://github.com/basic-automation/weftdb/pull/689.

### What the diff does

Every case previously ingested into one sealed segment and the cross-segment merge
(`weft_physical_type::merge_newer_wins` in `read.rs:190`) was never reached. The diff:

- Declares upsert keys (`host_key()`) via `declare_with_upsert_keys` so overlapping
  writes are recognised as updates.
- Adds `seal_counts()` and `assert_sealed_overlap()` helpers.
- Cases 1, 2a, 2b, 2c, 2d assert two sealed segments via these helpers.
- Case 3 asserts series A holds 2 segments, series B holds 1, plus a value-range
  raw-scan assertion.
- New `single_segment_has_nothing_to_merge` — one ingest, nothing to merge.
- New `seal_order_is_the_conflict_rule_not_write_order` — pins seal-order wins.

### Verified

- `git merge origin/main` fast-forward to 49ee01ac — done.
- `cargo test -p weftdb --all-features --locked -j 1 --lib overlap --no-run` — compiled.
- `git status --porcelain` — only the one modified file, no staged artifacts.

### Not verified (must check first in CI)

The host lacked memory to run the actual tests. The first thing a follow-up must
check:

1. `cargo test -p weftdb --all-features --locked -j 1 --lib overlap`
   — each new and modified test must pass. The critical one is whether
   `assert_sealed_overlap` really observes two sealed segments (it asserts
   `ours == 2`). If the store ever consolidates index rows at open (it does not
   currently — confirmed by reading the open path), this assertion fails.
2. `cargo test -p weftdb --all-features --locked --lib` — no regressions.
3. `cargo clippy -p weftdb --all-targets --all-features --locked -- -D clippy::correctness -D clippy::suspicious -D clippy::perf`
   — no new lints.

### Surprises documented in PR body

1. No memtable exists; every ingest seals immediately.
2. Seal order is the one conflict dimension — the newer-in-older-segment case
   cannot be expressed.
3. `declare_with_upsert_keys` is forward-compatible with untagged ingests.

## Handoff — 2026-11-14 — ROB-20 (#445)

### What was delivered

- `docs/maintenance-budgets-plan.md` (new) — full audit + gap table + 5 ordered slices
- Append to end of `DECISIONS.md` — entry dated 2026-11-14

### Statuses found

| Behaviour | Status | Key evidence |
|---|---|---|
| Maintenance resource budgets | Most mechanisms missing | Disk guard excludes maint writes (`seal.rs:1093` vs `swap_plans` at `seal.rs:348`); no byte/memory/time budget env variables; `MaintenanceBreaker` is a circuit breaker for failures, not a resource budget |
| k-way merge | Not shipped | `fold_newest_wins` calls `merge_newer_wins` pairwise in serial loop (`segment_store.rs:1065-1073`); no k-way/priority-queue merge exists |
| Re-splitting of oversized frames | Partial — overlap-split exists, auto-resplit missing | `reconcile_overlaps_with_policy` (`maint.rs:1071`) splits cold prefix off; `split_segment` (`maint.rs:634`) is operator-only, never daemon-called |

### Next concrete step (after review)

1. Read the plan and decisions entry in a PR.
2. Brief Slice 1 (disk guard coverage for maintenance writes, G2) as a single crate `weftdb` change.

### Changed files (local, not yet committed)

- `docs/maintenance-budgets-plan.md` (new)
- `DECISIONS.md` (append)

### Not changed, as instructed

- `ROADMAP.md` (unticked)
- `CHANGELOG.md` (no `docs:` entries found)
- No `.rs` files