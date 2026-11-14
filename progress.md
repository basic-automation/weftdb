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

## Handoff — 2026-11-14

### Deliverable

`weft-server/tests/fre10a_contract.rs` — contract tests for the first half of the
73-route registry (Probe 4, Metrics 5, Compute 17). House style of `fre10b_contract.rs`.

### Files changed

- `weft-server/tests/fre10a_contract.rs` (created, 611 lines)
- `ROADMAP.md` (line 88 ticked: `- [x] FRE-10a ...`)
- `DECISIONS.md` (section appended)

### Commit

- Commit `efebbef` on `p12/fre10a-contract-tests`, subject `fre10a: contract tests for probes, compute endpoints, metrics and roles (#325)`, signed-off.
- Pushed to `origin/p12/fre10a-contract-tests`.
- PR opened at https://github.com/basic-automation/weftdb/pull/744.
- PR body states: "CI is the first execution; no cargo build/check/test/clippy/miri was run locally."

### What the tests do

- **Probe** (4 routes): `health_answers_its_frozen_shape`, `ready_answers_its_frozen_shape` —
  pin the key sets of HealthResponse and ReadyResponse.
- **Metrics** (5 routes): `metrics_answers_its_frozen_shape` (Prometheus exposition
  format), `profile_current_answers_its_frozen_shape` (JSON with six endpoint sections).
- **Compute** (17 routes): `interpolate_json_answers_its_frozen_shape`,
  `downsample_json_answers_its_frozen_shape`, `interpolate_point_answers_its_frozen_shape`,
  `compute_formats_carry_their_media_types`, `ilp_compute_formats_carry_their_media_types`.
- **Gate** (26 routes × 7 principals): `the_gate_holds_for_every_route_in_the_families`
  and `check_route_gate`.
- **Coverage partition**: `the_fre10a_families_are_the_frozen_size`,
  `the_contract_coverage_spans_the_whole_registry`.

### Hard rules honoured

- No `cargo build/check/test/clippy/miri` — only `cargo +nightly fmt`.
- No edits to `fre10b_contract.rs`, `routes.rs`, `openapi.rs`, `compat/openapi/weftdb.json`,
  `docs/config-schema.md`.
- Counts derived from `build_registry()`, not copied from `fre10b_contract.rs`.

### First CI run will fail

The PR body says CI is the first execution. The test file may have compilation errors
or latent failures that only `cargo test` reveals. Follow-up must check:

1. `cargo test -p weft-server --test fre10a_contract --all-features --locked -j 1`
2. `cargo test -p weft-server --all-features --locked --test fre10b_contract` (no regressions)
3. `cargo test -p weft-server --all-features --locked --lib` (no regressions)
4. `cargo +nightly fmt --all --check`