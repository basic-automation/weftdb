# Progress — GPU-FLAKE (#725)

## Handoff — 2026-11-14

### Changed files

- `weft-bench/src/gpu_memory.rs` (16 insertions, 2 deletions)
- `CHANGELOG.md` (6 insertions)
- `ROADMAP.md` (1 insertion, 1 deletion)
- `DECISIONS.md` (21 insertions)

### Commit

- Commit pending (below).  Subject: `[GPU-FLAKE] Make the GPU pool bound test contention-tolerant (#725)`
- Branch `p12/gpu-flake-pool`, to push to `origin/p12/gpu-flake-pool`.
- PR title: `[GPU-FLAKE] Make the GPU pool bound test contention-tolerant (#725)` — closes #725, do NOT merge.

### What the diff does

The test `gpu_memory::tests::the_pool_stays_bounded_and_stable_on_a_real_gpu` flaked when the CI
runner's GPU was under contention (14 GB in use by other work) because the pool allocated one buffer
after round one, making `peak_later > peak_first` and `stable == false`, which then caused
`result.correctness.passed()` to return false.

The fix keeps the workload's `output_count_ok` definition unchanged (it truthfully records whether
the pool was stable) and **relaxes only the test** to check the hard safety invariants individually:

- Every output value is finite.
- Every call returned the expected number of points.
- The pool never exceeds its configured cap (`max_pool_bytes`).
- The device allocator does not grow without bound.

Also:
- CHANGELOG.md: `### Fixed` entry under `## [Unreleased]`.
- ROADMAP.md: ticked the matching line "Flaky under GPU contention…".
- DECISIONS.md: appended `## 2026-11-14 — GPU pool bound test… (#725)` section.

### Rules observed

- `cargo build/check/test/clippy` was NOT run (host has ~8 GB free; a build OOM-kills the local
  model server; CI is the first execution — stated in the PR body).
- No `rustfmt` call needed (the changed code matches the crate's 2021 edition style without
  formatting changes — no `--edition 2024` used).
- No new crate features, no new env variables.
- Not touching `docs/config-schema.md`, `config/schema.toml`, `weft-server/src/settings.rs` or
  `compat/openapi/weftdb.json`.

### Not verified (must check first in CI)

Compilation and test pass must be validated in CI. The diff is purely assertion-logic changes in a
`#[cfg(test)]` block plus doc-only changes.

### Surprises documented in PR body

1. The `output_count_ok` field conflates pool-management observations with output correctness. The
   decision was to change only the test, not the schema field, so stored artifacts preserve their
   existing interpretation.