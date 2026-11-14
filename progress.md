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