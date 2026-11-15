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
## Handoff — PR #741 (ROB-18 CI fix) — 2026-11-14

### Per-job diagnosis

| Job | Outcome | Failure | Root cause |
|---|---|---|---|
| `test (ubuntu-latest)` | FAILED | `ingest_admission_refuses_over_cap_and_no_headroom` at line 1123 | `post_text()` omits Content-Length; 413 declared-length check is skipped; store with 1<<60 reserve refuses every reservation as 507 |
| `test (macos-latest)` | FAILED | same test | same cause |
| `test (windows-latest)` | FAILED | same test | same cause |
| `rustfmt` | FAILED | `use` ordering stale | already fixed by commit 35e455c (fmt --all) |

### Changes pushed

| Commit | SHA | File | Summary |
|---|---|---|---|
| 0a8d9d5 | `[ROB-18] Fix the 413 admission test: send Content-Length so the declared-length check fires (#721)` | `weft-server/tests/fre10b_contract.rs` | Replace `post_text()` with manual Request building with explicit Content-Length header |

### Remaining work

- Wait for CI to re-run on the pushed commit.
- If the 413 test passes on all three platforms, the CI gate is green.
- If other tests fail, diagnose and repeat.

### Resume commands

```bash
cd /mnt/deepmem/wt/p12-721
gh run list --branch p12/rob-18-body-deadline --limit 5 --json conclusion,status,databaseId,displayTitle
gh run view <run-id> --log-failed 2>&1 | tr '\r' '\n' | grep -i 'panicked\|failures:\|error\[\|assertion'
git diff HEAD~1 HEAD  # review last change
```
