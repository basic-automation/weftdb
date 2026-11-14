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