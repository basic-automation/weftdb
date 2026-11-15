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

## Handoff — ROB-22 (suspect-frame policy + decode breaker), 2026-11-14

### Changed files (both commits)

- `weftdb/src/types/quarantine.rs` — `SuspectFramePolicy`, `decode_failure_action`, tests
- `weftdb/src/types/segment_store.rs` — `decode_breaker` field on `SegmentStoreOptions` and `SegmentStore`,
  `decode_breaker_state`, `DEFAULT_DECODE_BREAKER_FAILURES`, `DEFAULT_DECODE_BREAKER_COOLDOWN`,
  `MaintenanceBreaker::decode_from_env()`
- `weftdb/src/types/segment_store/open.rs` — constructor wiring for `decode_breaker` and `decode_breaker_state`
- `weftdb/src/types/segment_store/maint.rs` — `decode_breaker_permits`, `decode_breaker_record`,
  `decode_breaker_count_failure` methods
- `weft-server/src/settings.rs` — `WEFT_DECODE_BREAKER_COOLDOWN_SECS`, `WEFT_DECODE_BREAKER_FAILURES`,
  `WEFT_SUSPECT_FRAME_POLICY` in VARS array with strict validation; `NON_NEGATIVE_INTS` updated
- `docs/config-schema.md` — version-history row updated, three new Tuning rows, count raised 46→49
- `ROADMAP.md:92` — ticked
- `docs/roadmap-notes.md` — ROB-22 section

### Branch & PR

- Branch `p12/rob-22-suspect-frames` pushed to origin.
- PR #740 opened at https://github.com/basic-automation/weftdb/pull/740
  — body says nothing was compiled or run locally, and lists what CI must confirm.

### What is done

1. **SuspectFramePolicy** (`quarantine.rs`): parse, Display, `decode_failure_action` (corrupt →
   always quarantine, suspect → follow policy), four test groups (parse round-trip, display
   round-trip, suspect respects policy, corrupt always quarantines, C5-named causes).
2. **Decode breaker** (`segment_store.rs`, `open.rs`, `maint.rs`): `SegmentStoreOptions::decode_breaker`
   (separate from maintenance breaker), `decode_breaker_state` map, `decode_from_env()` method,
   `decode_breaker_permits` (returns false while tripped), `decode_breaker_record` (records success
   → clears, failure → increments and trips at threshold), `decode_breaker_count_failure`.
3. **Env-var registration** (`settings.rs`): three variables in sorted position with PAC-5b closed-enum
   validation for `WEFT_SUSPECT_FRAME_POLICY` and non-negative-integer validation for the others.
4. **Config docs** (`config-schema.md`): version-history row, new Tuning rows, counts updated.
5. **Roadmap metadata**: ROADMAP.md ticked, roadmap-notes.md section written.

### What CI must confirm

1. `cargo test -p weftdb --lib quarantine` — SuspectFramePolicy tests pass
2. `cargo test -p weft-server -- settings` — tier assertions and coverage walk pass
3. `cargo check -p weftdb -p weft-server` — type checking
4. `scripts/config/check_schema.py` — schema validation
5. `cargo fmt --check`

### Deliberately left out

- **Integration of the decode breaker into daemon sweep entry points and the interactive read path.**
  The first worker stopped at this exact decision point. The breaker API (permits, record,
  count-failure) mirrors the maintenance breaker so the wiring pattern is the same. The daemon
  code in `weft-server/src/*daemon*.rs` should check `decode_breaker_permits(aspect)` before
  attempting a decode and call `decode_breaker_record(aspect, failed)` after. The right injection
  point needs the daemon author's input — it was too speculative to commit without it.
- **No HTTP routes** — PR #213 owns routes.rs, openapi.rs and the OpenAPI golden.
- **No `cargo build/check/test/clippy`** — the host has insufficient memory; CI is the compiler.

## Handoff — CI fix round, 2026-11-14

### What changed

- `weftdb/src/types/segment_store/maint.rs` — wired `decode_breaker_permits` as gate and
  `decode_breaker_record` as success/failure callback into all three store-wide maintenance
  sweeps (`reconcile_all_over_threshold_with_cancel`, `reconcile_all_hot_cold_with_cancel`,
  `reconcile_all_overlaps_with_cancel`)
- `weftdb/src/types/quarantine.rs` — fixed `Self::classify_decode_failure` → `classify_decode_failure
  (rustdoc resolution); shortened first doc paragraph on `SuspectFramePolicy` and
  `decode_failure_action` (clippy `too_long_first_doc_paragraph`); eliminated `bytes.clone()`
  redundant clone (clippy `redundant_clone`)
- `weftdb/src/types/segment_store.rs` — shortened first doc paragraph on
  `DEFAULT_DECODE_BREAKER_FAILURES` (clippy `too_long_first_doc_paragraph`)

### Commit

- `28ba54c` on `p12/rob-22-suspect-frames`, signed-off, pushed.

### What remains

- **CI must re-run all 8 failing checks to confirm green.** The dead-code fix and rustdoc fix
  are syntax-level; the clippy issues are doc-string-level. Only pushes from here trigger CI,
  and the checks auto-run.
- **No local verification** — host memory limits prevented clippy/rustdoc from running locally.
  Build compiles (`cargo check -p weftdb -j 1` passes).

### Resume commands

```
cd /mnt/deepmem/wt/p12-720
git log --oneline -1
gh pr checks 740     # wait for CI to finish
gh run view --log-failed <run-id>   # if any still fail
```

## Step Zero — maintenance sweep Err arm analysis

### The three sweep `Err` arms (maint.rs ~:874-880, ~:1021-1027, ~:1264-1270)

Each arm catches errors from `reconcile_aspect_if_unsorted_exceeds_held`,
`reconcile_aspect_hot_cold_held`, or `reconcile_overlaps_held`. These methods
internally call `read_bound_frame` → `frame_read::read_full`, which is the frame-decode
call site. But the `Err` arm also catches libSQL MVCC conflicts, filesystem errors,
adoption failures, and any other error that arises during reconciliation.

**Finding**: the sweep `Err` arm is a *reconcile* failure catcher, not a *frame-decode*
catcher. The frame decode is one of several failure modes. Hanging the policy on the
blanket `Err` arm would apply it to non-decode errors, which is wrong per the brief.

### The three fsck `classify_decode_failure` sites (segment_store.rs:2394, 2572, 2690)

These are startup recovery code and do NOT go through the maintenance sweep. The brief
says recovery MUST NOT consult the policy (a `skip` must not let recovery drop a frame
the index will need). The per-rule comment on `SuspectFramePolicy` already says this.

### Where the policy should be wired

The interactive read path (`read_time_range_of` in `read.rs:126`) reads each segment's
frame and calls `frame_read::read_range(…)`. On decode failure, the `?` propagates the
`FrameFaultError` to the caller. This is the site to consult `decode_failure_action`:

- **Fail**: propagate the error (today's behaviour, unchanged).
- **Quarantine**: propagate the error (default = byte-identical to today; the quarantine
  action is reserved for the fsck path, which does not consult the policy).
- **Skip**: log the condition and `continue` the segment loop, returning data from the
  readable segments only.

### Breaker orthogonality
`decode_breaker_record` is called in the sweep loop arms on success/failure, correctly
counting every decode attempt regardless of disposition. No change needed.

### Decode breaker on reconcile Err arm?
The `decode_breaker_record` call on the `Err` arm records *reconcile* failures, which
include decode failures among other things. This is discussed in the original ROB-22
spec — the decode breaker is a coarse fuse, not frame-grained. No change to that in
this slice.

## Handoff — p12-720 policy wiring, 2026-11-14

### What changed

- `weftdb/src/types/segment_store.rs` — added `suspect_frame_policy` field to
  `SegmentStoreOptions` (next to `decode_breaker`), `DEFAULT_SUSPECT_FRAME_POLICY`,
  import of `SuspectFramePolicy` / `decode_failure_action`, `from_env` reads the
  env var, `with_suspect_frame_policy` builder; added `suspect_frame_policy` field
  to `SegmentStore` struct; added re-export of `SuspectFramePolicy` / `decode_failure_action`.
- `weftdb/src/types/segment_store/open.rs` — wired `suspect_frame_policy` from options
  into store constructor.
- `weftdb/src/types/segment_store/read.rs` — wired `decode_failure_action` into
  `read_time_range_of` so decode errors consult the policy.
- `weft-server/src/settings.rs` — fixed `read_by` string.
- `docs/config-schema.md` — fixed `read_by` column.

### Tests added

- **Variable-reaches-store tests**: env set → `from_env().suspect_frame_policy == …`,
  unset → default, garbage → default; opened `SegmentStore` exposes the same value.
- **Behaviour tests**: corrupt frame + Fail policy → read surfaces the decode error;
  Skip policy → read succeeds without the corrupt frame; Quarantine (default) →
  today's outcome (error propagated).