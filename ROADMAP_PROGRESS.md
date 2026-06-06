# DSP Roadmap Progress Log

Append-only handoff log for the nightly `dsp-dev-routine`. Newest entries at the
bottom. Each entry records: date · roadmap item · what changed · build/test/clippy
results (exact counts) · done-vs-open · next step · PR.

---

## 2026-06-05 — Routine bootstrapped

- **Item:** none yet (routine created).
- **State:** `ROADMAP.md` was reorganized around the benchmark-led commercial thesis;
  **DSP-Bench is the #1 track**. Workspace builds (`cargo check --workspace` green).
- **Next step (first run):** begin the Immediate Next Actions — create `dsp-bench` as
  a first-class workspace member, define the `interpolation-heavy-irregular` benchmark
  profile, and add a DSP adapter skeleton + result schema. Keep the workspace building;
  add one smoke test. Scope to a bounded slice and open a PR.

---

## 2026-06-05 (run 2) — DSP-Bench scaffold landed

- **Item:** Phase 1 / Track 1 — **DSP-Bench**. Immediate Next Actions #1 (workspace
  member) and #2 (`interpolation-heavy-irregular` profile); partial on #3 (DSP adapter)
  and #7 (p50/p95/p99 reporting).
- **What changed (new crate `dsp-bench`, wired into the root workspace):**
  - `dsp-bench/Cargo.toml` — new library member; workspace `members` updated in root
    `Cargo.toml`.
  - `src/profile.rs` — `InterpolationProfile` + the flagship
    `interpolation_heavy_irregular()` profile and a fully **seeded, reproducible**
    (`ChaCha8Rng`, published seed) dataset generator: irregular, jittered timestamps
    over a fixed epoch anchor with configurable missingness/gaps.
  - `src/adapter.rs` — vendor-neutral `SystemAdapter` trait (honors the connector
    hard-constraint: no vendor coupling in the harness core; concrete adapters stay
    decoupled).
  - `src/dsp_adapter.rs` — `DspAdapter` driving DSP's native engine
    (`splimes::auto_interpolate`).
  - `src/schema.rs` — serializable `BenchResult` (latency distribution + `DatasetMeta`
    + `CorrectnessReport`); correctness gates publishability (`is_publishable`).
  - `src/stats.rs` — nearest-rank p50/p95/p99 + min/max/mean/stddev latency summary.
  - `src/lib.rs` — `run_profile()` runner (N timed reps, fresh dataset clone per rep,
    correctness verdict, throughput) + smoke tests.
  - `dsp-bench/README.md` — crate purpose, current scaffold scope, what's not yet done.
  - `ROADMAP.md` — Immediate Next Actions #1/#2 marked ✅, #3/#7 marked 🟡 with notes.
  - **Summary:** the benchmark harness now has a real, compiling spine — a reproducible
    workload profile, a vendor-neutral adapter boundary, a working DSP adapter, a
    correctness-gated result schema, and percentile reporting. The end-to-end smoke
    test generates the irregular dataset, interpolates it through DSP, and asserts the
    grid is correctly sized + finite and that the schema round-trips through JSON.
- **Build/test/clippy (real, this run; nightly toolchain):**
  - `cargo build --workspace` — **GREEN** (baseline 1m37s; with new member 39.9s).
  - Workspace **unit tests** (`cargo test --workspace --lib`, `SKIP_SLOW_TESTS=1`):
    **79 passed, 0 failed** — database 21, database_orchestration 17, **dsp-bench 11**,
    dsp-tui 7, splimes 23.
  - `cargo clippy -p dsp-bench --all-targets` — **clean, 0 warnings in dsp-bench**
    (pedantic+nursery enabled in the crate). The 4 remaining `splimes` clippy warnings
    are **pre-existing** in untouched files (`gpu/mod.rs`, `helpers/batch.rs`,
    `splines/quadratic.rs`) — not introduced here. No `#![allow]` added; all dsp-bench
    lints fixed directly (doc backticks, `sort_by_key`, `mul_add`, `Eq` derive,
    `&'static str`, `# Panics`) with targeted `#[allow]` only at intentional numeric
    casts in the stats/generator math.
  - `cargo fmt -p dsp-bench --check` — clean (repo rustfmt: hard tabs, max_width 10000).
  - **Time-boxed out:** the `database` `tests/db_tests.rs` GPU **integration** suite is
    long-running (multiple `analyze_point` GPU tests >60s each) and does not honor
    `SKIP_SLOW_TESTS`; it was stopped to stay in budget. It is **pre-existing and
    unrelated** to this isolated new crate; the workspace unit suite above is green.
- **Done vs open:** DONE — workspace member, reproducible profile+generator, adapter
  trait, DSP adapter, result schema, percentile stats, runner, smoke tests, docs.
  OPEN — DuckDB + competitor adapters; InfluxDB Line Protocol ingest; more workloads
  (range fetch, downsample, compression); dataset corpora; bootstrap CIs; report
  runners (JSON/Parquet/HTML); methodology doc.
- **Next step:** add the **DuckDB adapter** (first competitor baseline, CPU-only) behind
  the same `SystemAdapter` trait + a JSON report writer for `BenchResult`, so
  `run_profile` results can be persisted as artifacts. Then InfluxDB Line Protocol ingest.
- **PR:** https://github.com/physics515/DSP/pull/3
