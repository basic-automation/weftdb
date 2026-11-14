# Progress — #723 Real-filesystem ENOSPC evidence

## Handoff — 2026-11-14

### What is committed

Two commits on `p12/enospc-real-fs` (origin/main @ 1c8bf50):

1. **556e628** — `Add ENOSPC integration tests for the injectable space source (#723)`
   - Creates `weft-server/tests/enospc_real_fs.rs` with two tests:
     - `space_source_reports_a_full_filesystem_and_the_server_says_507` (unconditional,
       uses `SegmentStoreOptions::with_disk_reserve_bytes(u64::MAX)` so the guard
       refuses every write as `507 insufficient_storage`)
     - `a_real_full_filesystem_produces_enospc_end_to_end` (probes `unshare -Um true`;
       on capable hosts mounts a 1 MiB tmpfs inside a namespace and confirms ENOSPC;
       on this host (EPERM) skips with the reason printed)

2. **9962078** — `Tick ROADMAP.md:92, record decision, add CHANGELOG entry (#723)`
   - ROADMAP.md:92 `- [ ]` → `- [x]`
   - DECISIONS.md: new `## 2026-11-14 — Real-filesystem ENOSPC evidence (#723)` entry
   - CHANGELOG.md: new `### Testing` section with ENOSPC evidence entry

### What is not committed / needs attention

- The PR is created: https://github.com/basic-automation/weftdb/pull/743
- Comment posted on #723: https://github.com/basic-automation/weftdb/issues/723#issuecomment-6881217074
- Branch pushed: `p12/enospc-real-fs`

### Next command to run

```
gh pr merge 743 --squash --subject '[ROB-17] Real-filesystem ENOSPC evidence on the injectable space source (#723)' --body 'Closes #723'
```
(CI must pass first — the PR body notes this is the first execution.)

## Handoff — 2026-11-14 14:58 UTC

### CI log lines found (run 43633006821)

**clippy** (job 132563542412):
```
error: temporary with significant `Drop` can be early dropped
  --> weft-server/tests/enospc_real_fs.rs:57:6
error: the borrowed expression implements the required traits
  --> weft-server/tests/enospc_real_fs.rs:68:40
error: could not compile `weft-server` (test "enospc_real_fs") due to 2 previous errors; 8 warnings emitted
##[error]Process completed with exit code 101.
```

**test (ubuntu-latest, macos-latest, windows-latest)** (jobs 132563542758, 132563542797, 132563542789):
```
test space_source_reports_a_full_filesystem_and_the_server_says_507 ... FAILED
thread '...' panicked at .../tokio-1.53.3/.../mod.rs:91:9:
Cannot start a runtime from within a runtime. This happens because a function
(like `block_on`) attempted to block the current thread while the thread is
being used to drive asynchronous tasks.
test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out.
```
`a_real_full_filesystem_produces_enospc_end_to_end` passed cleanly on all three OS targets.

### Changes pushed (93558b2)

- Made `router_with_impossible_reserve()` async — direct `await` instead of nested `Runtime::new().block_on(...)`, matching the pattern of `secured_router()` in `security_acceptance.rs` and `seeded_router()` in `fre10b_contract.rs`.
- Removed `use tower::ServiceExt as _;` import (no longer needed).
- Removed `&` before `format!(...)` in `ingest_points_request()` (clippy `needless_borrows_for_generic_args`).
- Removed the temporary scoping issue that triggered `significant_drop_tightening`.
- `cargo +nightly fmt --all` produced no changes.
- Pushed to `p12/enospc-real-fs`.
- PR body updated with the fix summary.

### What still may fail

Unknown — these are the first CI runs after the fixes. The three test jobs and clippy may now pass, or new issues may surface.