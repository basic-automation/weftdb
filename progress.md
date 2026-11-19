# Progress: p800/count-read-parity - Count/Read parity for overlapping segments (#800)

## Done

- Read brief, issue #800, key files (legacy_seal.rs, outputs.rs, traits/outputs.rs, maint.rs, legacy_seal_tests.rs)
- Created branch p800/count-read-parity from origin/main
- Commit 9eff2c5: Added test `count_agrees_with_range_read_after_overlapping_ingests` (red on origin/main — count=6 vs read=4)
- Commit 69a1895: Fix — `ingest_batch_with_gate` now calls `store.reconcile_overlaps(&key)` after every ingest
- Added rustdoc invariant on `get_measurements_count` in trait and impl
- Added DECISIONS.md section
- rustfmt and check-serial-order pass
- Pushed to origin, PR #813 opened
- CI run #44467522997 (queued)

## Handoff

Branch: p800/count-read-parity (2 commits ahead of origin/main)
PR: https://github.com/basic-automation/weftdb/pull/813
CI: https://github.com/basic-automation/weftdb/actions/runs/44467522997

If CI fails, check the run output for compile errors and fix on the same branch.
If CI passes, the fix is complete — do not merge (task specifies DO NOT merge).

