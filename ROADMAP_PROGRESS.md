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

## 2026-06-05 — Dependency refresh + Turso 0.4 → 0.6

- **Item:** Dependency hygiene + control-plane engine upgrade (supports Track 5
  Storage v2 / Phases 7–8). Separate branch off `main` (independent of the DSP-Bench
  scaffold PR #3).
- **What changed:**
  - `cargo update` — refreshed **all 163 semver-compatible** dependencies (incl.
    `tokio` 1.49 → 1.52, `chrono` 0.4.43 → 0.4.45, `ratatui` → 0.30.1, `sysinfo` →
    0.39, `serde_json`, `bytemuck`, futures, etc.).
  - Root `Cargo.toml`: `turso` pin `0.4` → **`0.6`** (locked at 0.6.1 across all
    `turso_*` crates). 0.6 is the "all major SQLite features supported" release.
  - **Turso 0.6 migration:** 0.6 rejects `AUTOINCREMENT` under
    `journal_mode=experimental_mvcc` at parse time (0.4 tolerated it). Migrated **32**
    control-plane schema definitions from `INTEGER PRIMARY KEY AUTOINCREMENT` →
    `INTEGER PRIMARY KEY` in `database/src/types/aspect.rs` (29) and
    `database/src/types/database/inputs.rs` (3). Still a rowid alias that auto-assigns
    on insert; DSP never depended on the no-reuse guarantee. MVCC `BEGIN CONCURRENT`
    write path unchanged.
  - `ROADMAP.md`: new section **"Control-plane engine: Turso/libSQL 0.6 adoption"**
    (+ TOC entry) — documents the migration and maps 0.6 features to phases: adopt
    (production MVCC, encryption-at-rest → P8, `VACUUM INTO` backups → P7.4, triggers
    for audit/invariants → P7/8, `Statement::n_change()` → P3/7, dynamic auth tokens →
    P8); evaluate (vector/embedding search → P9 only, CDC/sync → P7); defer/avoid
    (multi-process WAL — incompatible with `BEGIN CONCURRENT`; never make Turso the
    measurement store, per hard-constraint #3).
- **Build/test/clippy (real, this run; nightly):**
  - `cargo build --workspace` — **GREEN** against Turso 0.6.1 (1m44s); core
    `Database`/`Connection`/`execute`/`query`/`params!` API unchanged — no code changes
    needed beyond the schema migration.
  - Workspace **unit tests** (`cargo test --workspace --lib`, `SKIP_SLOW_TESTS=1`):
    **68 passed, 0 failed** — database 21, database_orchestration 17 (these exercise
    Turso MVCC pipelines, and were the 3 that failed pre-migration), dsp-tui 7,
    splimes 23. *(Before the AUTOINCREMENT fix: 3 orchestration tests failed with the
    MVCC parse error — now fixed.)*
  - `cargo clippy --workspace` — **no new deprecation/unused warnings** from the dep
    bumps (0 `deprecated`, 0 unused-import). Remaining pedantic/nursery counts
    (database 127, orchestration 32, dsp-tui 13, splimes 4) are **pre-existing** in each
    crate's own lint config, unrelated to this change.
  - **Not run:** `database/tests/db_tests.rs` GPU integration suite (long-running, as in
    the prior run); the Turso-heavy schema/pipeline paths are covered by the
    orchestration unit tests above.
- **Done vs open:** DONE — all deps refreshed, Turso on 0.6.1, MVCC schema migration,
  feature-adoption doc. OPEN — actually wiring the adopted 0.6 features (encryption,
  `VACUUM INTO` backups, audit triggers) lands in their mapped phases (7/8).
- **Next step:** when Phase 7/8 work begins, implement `VACUUM INTO` control-plane
  backups and at-rest encryption for catalog/metadata DBs.
- **PR:** https://github.com/physics515/DSP/pull/4

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

---

## 2026-06-06 — DSP-Bench JSON report runner

- **Item:** Phase 1 / Track 1 — **DSP-Bench**. First half of the prior run's stated
  next step ("a JSON report writer for `BenchResult` so `run_profile` results can be
  persisted as artifacts"). Delivers the `reports/json/` slice of the DSP-Bench layout
  and the roadmap's fair-protocol "keep raw results" reproducibility requirement.
  (The DuckDB adapter — the other half — is intentionally deferred: it needs the heavy
  native `duckdb` crate and is too risky for a time-boxed run; tracked as next step.)
- **What changed (new module in `dsp-bench`, no new dependencies):**
  - `dsp-bench/src/report.rs` — new module:
    - `RunMetadata` — lightweight, dependency-free environment capture (`dsp-bench`
      version via `CARGO_PKG_VERSION`, target OS/arch via `std::env::consts`, and an
      injected `generated_at` RFC-3339 timestamp so construction stays deterministic
      and testable). Honestly scoped: full hardware capture (CPU/RAM/GPU/drivers) is
      documented as a later increment.
    - `BenchReport` — envelope `{ schema_version, metadata, results: Vec<BenchResult> }`
      holding one or many results (e.g. a multi-adapter comparison) as a single
      self-describing artifact. `new`/`with_results`/`push`, `to_json_pretty`,
      `write_json` (creates missing parent dirs, writes pretty JSON), plus
      `is_publishable` (gated: empty → false, any failing result → false) and
      `publishable_count`.
    - `default_filename(profile, adapter)` — sanitized `…__….json` artifact name
      (non-`[A-Za-z0-9._-]` → `-`, empty component → `unnamed`).
  - `dsp-bench/src/lib.rs` — `pub mod report;`, re-exported `BenchReport`/`RunMetadata`,
    crate-doc bullet, and a new end-to-end test wiring a real `run_profile` result →
    `BenchReport` → `write_json` to a unique temp path → read back → parse → assert
    publishable.
  - `dsp-bench/README.md` — documented the JSON report runner under Status; narrowed
    the "Not yet" list accordingly (Parquet/HTML formats + full hardware capture
    remain open).
  - **Summary:** DSP-Bench results are now durable, inspectable JSON artifacts with a
    correctness-gated publishability check at the report level, not just per result.
- **Build/test/clippy (real, this run; nightly `rustc 1.96.0-nightly`):**
  - `cargo build --workspace` — **GREEN** (full workspace; the new module compiled in
    1m12s on top of the warm workspace; clean baseline confirmed green before edits).
  - `cargo test -p dsp-bench` — **17 passed, 0 failed, 0 ignored** (was 11; +6:
    5 in `report::tests` — `capture_fills_version_and_target`,
    `report_round_trips_through_json`, `publishability_requires_all_results_to_pass`,
    `write_json_creates_parents_and_round_trips_from_disk`,
    `default_filename_is_sanitized` — plus 1 e2e in `lib::tests`,
    `run_result_persists_as_a_json_report_artifact`). 0 doc-tests.
  - `cargo clippy -p dsp-bench --all-targets` — **0 warnings in dsp-bench** (pedantic +
    nursery enabled in the crate). The only clippy output is the **4 pre-existing**
    `splimes` warnings (`gpu/mod.rs`, `helpers/batch.rs` ×?, `splines/quadratic.rs`) in
    untouched files — identical to the prior run, not introduced here. No `#![allow]`
    added.
  - `cargo fmt -p dsp-bench --check` — clean (applied repo rustfmt: hard tabs).
  - **Not run:** full `cargo test --workspace` and the `database` `tests/db_tests.rs`
    GPU integration suite (long-running, time-boxed out as in prior runs). This change
    is isolated to the `dsp-bench` crate — no `splimes`/`database`/orchestration code
    was touched — so the per-crate suite above fully covers it; the workspace build is
    green.
- **Done vs open:** DONE — JSON report runner (`BenchReport` + `RunMetadata` +
  `write_json` + filename helper), report-level publishability gate, e2e
  run→report→disk test, docs. OPEN — DuckDB adapter (next), then ILP ingest; richer
  report formats (Parquet/HTML); full hardware capture in run metadata; bootstrap CIs;
  more workloads/datasets; methodology doc.
- **Next step:** add the **DuckDB adapter** (first competitor baseline, CPU-only)
  behind the existing `SystemAdapter` trait, then have a small runner emit a
  multi-adapter `BenchReport` (DSP + DuckDB) to `reports/json/`. Then InfluxDB Line
  Protocol ingest.
- **PR:** https://github.com/physics515/DSP/pull/5

---

## 2026-06-07 — DSP-Bench bootstrap confidence intervals

- **Item:** Phase 1 / Track 1 — **DSP-Bench**; roadmap "Immediate next actions" #7
  ("p50/p95/p99 + confidence-interval reporting") and the Phase 1.1 fair-protocol
  requirement "report median/mean/stddev and p50/p95/p99/max **with bootstrap CIs**".
  The percentile/summary half landed earlier; this run delivers the bootstrap-CI half,
  taking #7 from 🟡 to ✅. Chosen over the long-deferred DuckDB adapter because it is
  pure, dependency-free math (the crate already had `rand`/`rand_chacha`), fully
  self-contained to `dsp-bench`, and low-risk for a time-boxed run — whereas the DuckDB
  adapter still needs the heavy native `duckdb` crate.
- **What changed (isolated to `dsp-bench`, no new dependencies):**
  - `src/stats.rs` — new seeded nonparametric bootstrap:
    - `BootstrapConfig { resamples, confidence, seed }` (+ `Default`: 1000 resamples,
      0.95, fixed seed) — all three knobs published for exact reproducibility.
    - `ConfidenceInterval { point_ns, lower_ns, upper_ns }` (invariant
      `lower <= point <= upper`, enforced by clamping the point into the resampled
      spread).
    - `LatencyCis { resamples, confidence, seed, mean, p50, p95, p99 }`.
    - `LatencyStats::bootstrap_cis(samples, &config)` — resamples with replacement via a
      `ChaCha8Rng` seeded from `config.seed` (one shared stream; computes all four
      statistics per resample), reads each interval off the bootstrap distribution by
      the percentile method. Empty / zero-resample inputs yield zeroed intervals; a
      constant sample yields a degenerate interval.
  - `src/schema.rs` — `BenchResult` gains `latency_ci: Option<LatencyCis>`
    (`#[serde(default, skip_serializing_if = "Option::is_none")]`); `SCHEMA_VERSION`
    bumped 1 → 2. The field is additive + optional, so **v1 artifacts still deserialize**
    (covered by a new `v1_artifact_without_latency_ci_still_deserializes` test).
  - `src/lib.rs` — `run_profile` now populates `latency_ci` using a CI seed derived from
    the dataset seed (`profile.seed ^ 0xC0FFEE15C0DE`) so it is reproducible yet
    decoupled from the dataset-generation RNG stream; re-exports
    `BootstrapConfig`/`ConfidenceInterval`/`LatencyCis`; the e2e test asserts the CIs are
    present, ordered, and survive the JSON round-trip.
  - `README.md` — documented bootstrap CIs under Status; removed them from "Not yet".
  - `ROADMAP.md` — Immediate next action #7 🟡 → ✅.
- **Build/test/clippy (real, this run; nightly `rustc 1.96.0-nightly`):**
  - `cargo build --workspace` — **GREEN** (clean baseline confirmed green before edits;
    re-verified green after, 1.85s incremental).
  - `cargo test -p dsp-bench` — **23 passed, 0 failed, 0 ignored** (was 17; +6:
    `bootstrap_is_deterministic_for_a_given_seed`, `bootstrap_seed_changes_the_interval`,
    `bootstrap_interval_brackets_the_point_and_is_ordered`,
    `bootstrap_of_constant_sample_is_degenerate`,
    `bootstrap_of_empty_or_zero_resamples_is_zeroed`,
    `v1_artifact_without_latency_ci_still_deserializes`). 0 doc-tests.
  - `cargo clippy -p dsp-bench --all-targets` — **0 warnings in dsp-bench**. The only
    clippy output is the **4 pre-existing** `splimes` `unnecessary_sort_by` warnings
    (`gpu/mod.rs`, `helpers/batch.rs`, `splines/quadratic.rs`) in untouched files —
    identical to prior runs, not introduced here. No `#![allow]` added.
  - `cargo fmt -p dsp-bench --check` — clean (repo rustfmt: hard tabs, max_width 10000).
  - **Not run:** full `cargo test --workspace` and the `database` `tests/db_tests.rs`
    GPU integration suite (long-running, time-boxed out as in prior runs). This change
    is isolated to `dsp-bench` — no `splimes`/`database`/orchestration code touched — so
    the per-crate suite fully covers it; the workspace build is green.
- **Done vs open:** DONE — seeded bootstrap CIs for mean + p50/p95/p99, wired into
  `run_profile`/`BenchResult`, schema v2 with v1 back-compat, docs. OPEN — DuckDB adapter
  (next), then ILP ingest; richer report formats (Parquet/HTML); full hardware capture in
  run metadata; more workloads/datasets; methodology doc.
- **Next step:** add the **DuckDB adapter** (first competitor baseline, CPU-only) behind
  the existing `SystemAdapter` trait, then emit a multi-adapter `BenchReport` (DSP +
  DuckDB, each with latency + CIs) to `reports/json/`. Then InfluxDB Line Protocol ingest.
- **PR:** https://github.com/physics515/DSP/pull/6

---

## 2026-06-07 — DSP-Bench end-to-end timing spans

- **Item:** Phase 1 / Track 1 — **DSP-Bench**; roadmap "Immediate next actions" #6
  ("Add end-to-end timing spans"). Chosen over the long-deferred DuckDB adapter (#3,
  🟡) for the same reason prior runs deferred it: the `duckdb` crate is a heavy native
  C++ build (risky/slow on an unattended, time-boxed Windows run), whereas this slice
  is pure dependency-free Rust, fully self-contained to `dsp-bench`, and low-risk — a
  small, complete, verified increment. Takes #6 from not-started to 🟡 (harness-level
  spans done; deeper per-pipeline-stage spans remain for the instrumentation track).
- **What changed (isolated to `dsp-bench`, no new dependencies):**
  - `src/schema.rs` — new `TimingBreakdown { dataset_generation_ns, measured_ns,
    end_to_end_ns }` (Copy + Default), with `overhead_ns()` = saturating
    `end_to_end - dataset_generation - measured` (harness bookkeeping). `BenchResult`
    gains `timing: TimingBreakdown` as `#[serde(default)]` so pre-v3 artifacts (no
    `timing` key) still deserialize into a zeroed breakdown. `SCHEMA_VERSION` bumped
    2 → 3. Invariant by construction: `dataset_generation_ns + measured_ns <=
    end_to_end_ns`.
  - `src/lib.rs` — `run_profile` now times three spans via a `span_ns(Instant)` helper
    (saturating-into-`u64`): the one-time seeded dataset generation, the sum of the
    per-rep adapter calls (`measured_ns`, the operation under test), and the whole
    end-to-end span; populates `BenchResult.timing`. Re-exports `TimingBreakdown`. The
    per-rep latency loop reuses `span_ns` (drops the duplicated `as` cast).
  - `README.md` — documented the timing breakdown under Status (schema + runner).
  - `ROADMAP.md` — Immediate next action #6 → 🟡 with the scope note.
- **Build/test/clippy (real, this run; nightly `rustc 1.96.0-nightly` (55e86c996
  2026-04-02)):**
  - `cargo build --workspace` — **GREEN** baseline confirmed before edits (4m21s, cold
    target dir / build-dir lock contention at start). Change is isolated to `dsp-bench`;
    `cargo test -p dsp-bench` recompiled the crate + tests green after edits.
  - `cargo test -p dsp-bench` — **25 passed, 0 failed, 0 ignored** (was 23; +2:
    `schema::tests::timing_overhead_is_the_saturating_remainder`,
    `schema::tests::v2_artifact_without_timing_still_deserializes`). The lib e2e test
    `dsp_adapter_runs_interpolation_heavy_irregular` was extended to assert the timing
    spans are present, internally consistent (`end_to_end >= dataset_gen + measured`),
    that `measured_ns` matches `mean_ns * count` within integer-division rounding, and
    that `timing` survives the JSON round-trip. 0 doc-tests.
  - `cargo clippy -p dsp-bench --all-targets` — **0 warnings in dsp-bench**. The only
    clippy output is the **4 pre-existing** `splimes` `sort_by`/`unnecessary_sort_by`
    warnings (`gpu/mod.rs`, `helpers/batch.rs`, `splines/quadratic.rs`) in untouched
    files — identical to prior runs, not introduced here. No `#![allow]` added.
  - `cargo fmt -p dsp-bench --check` — clean (repo rustfmt: hard tabs, max_width 10000).
  - **Not run:** full `cargo test --workspace` and the `database` `tests/db_tests.rs`
    GPU integration suite (long-running, time-boxed out as in prior runs). This change
    is isolated to `dsp-bench` — no `splimes`/`database`/orchestration code touched — so
    the per-crate suite fully covers it; the workspace build is green.
- **Done vs open:** DONE — `TimingBreakdown` (dataset-gen / measured / end-to-end +
  overhead), wired into `run_profile`/`BenchResult`, schema v3 with v1+v2 back-compat,
  docs. OPEN — DuckDB adapter (next), then ILP ingest; richer report formats
  (Parquet/HTML); full hardware capture in run metadata; deeper per-pipeline-stage
  timing spans (instrumentation track); more workloads/datasets; methodology doc.
- **Next step:** add the **DuckDB adapter** (first competitor baseline, CPU-only)
  behind the existing `SystemAdapter` trait — consider gating it behind a cargo feature
  so the default workspace build stays free of the native `duckdb` dependency — then
  emit a multi-adapter `BenchReport` (DSP + DuckDB, each with latency + CIs + timing) to
  `reports/json/`. Then InfluxDB Line Protocol ingest.
- **PR:** https://github.com/physics515/DSP/pull/7

---

## 2026-06-08 — DSP-Bench InfluxDB Line Protocol parser

- **Item:** Phase 1 / Track 1–2 — **DSP-Bench**; roadmap "Immediate next actions"
  #5 ("Implement InfluxDB Line Protocol ingest") and backlog **B-ilp**. Chosen over
  the long-deferred DuckDB adapter (#3, 🟡) for the same reason prior runs deferred
  it — the `duckdb` crate is a heavy native C++ build, risky on an unattended,
  time-boxed Windows run — whereas this slice is pure dependency-free Rust, fully
  self-contained to `dsp-bench`, and a new file (so it did **not** collide with the
  then-open timing-spans PR #7 on creation; PR #7 has since merged and this branch
  was rebased/merged onto it cleanly). ILP is a wire *format*, not a vendor
  connector, so a format parser honors the connector hard-constraint; a concrete
  InfluxDB network connector still belongs outside the core. Takes #5 / B-ilp from
  🔴 (not-started) to 🟡 (format parser done; profile wiring + Phase-2 server
  endpoint remain).
- **What changed (isolated to `dsp-bench`, no new dependencies):**
  - `src/line_protocol.rs` — **new module.** A correct, dependency-free ILP parser:
    - `parse(&str) -> Result<Vec<LineRecord>, ParseError>` — full grammar:
      `measurement[,tags] fields [timestamp]`; tags + typed fields
      (`Float`/`Integer` `i`/`Unsigned` `u`/`Boolean`/quoted `Str`); `\,`, `\ `,
      `\=` escaping in the measurement/tag/key region and `\"`/`\` inside string
      values (a quote-aware, backslash-aware `split_unescaped` + `unescape`); blank
      and `#`-comment lines skipped; `ParseError` carries the 1-based line number.
    - `parse_points(&str, field, TimestampPrecision) -> Result<Vec<Point>, _>` — the
      harness on-ramp: projects records into sorted `splimes::Point`s on a chosen
      numeric field, skipping records that lack the field/timestamp or whose field
      is non-numeric. `TimestampPrecision::{Nanoseconds,Microseconds,Milliseconds,
      Seconds}` (ns default) scales raw integer stamps via checked-mul (overflow →
      `None`, no wrap) to a `DateTime<Utc>`.
    - `FieldValue::as_big_decimal()` coerces numeric fields to `BigDecimal`
      (NaN/inf → `None`), keeping the BigDecimal-as-logical-type rule.
  - `src/lib.rs` — `pub mod line_protocol;`, re-exports (`parse`, `parse_points`,
    `FieldValue`, `LineRecord`, `ParseError`, `TimestampPrecision`), and a module
    bullet in the crate docs. The merge with PR #7 unified the re-export block
    (both `line_protocol::*` and `TimingBreakdown` exported) and the crate-doc
    bullets; no logic conflict (PR #7 touched `schema.rs`/`run_profile`, this PR
    touched neither).
  - `dsp-bench/README.md` — documented the ILP parser under Status; refined "Not yet".
  - `ROADMAP.md` — Immediate next action #5 🔴→🟡 with scope note; backlog row
    **B-ilp** 🔴→🟡.
- **Build/test/clippy (real, at PR-creation; nightly `rustc 1.96.0-nightly`
  (55e86c996 2026-04-02)):**
  - `cargo build --workspace` — **GREEN** baseline confirmed before edits (18.62s
    incremental on a warm target dir).
  - `cargo test -p dsp-bench` — **34 passed, 0 failed, 0 ignored** (+11 new
    `line_protocol::tests`: full-line parse, no-tags/no-ts, every field type, quoted
    strings with spaces/commas/escaped quotes, escaped comma/space/equals in keys,
    comment/blank skipping, line-numbered error, empty-measurement/valueless-field
    rejection, precision scaling + overflow, sorted point projection with skip
    rules, numeric-coercion rejection). 0 doc-tests. (Post-merge re-verification
    below.)
  - `cargo clippy -p dsp-bench --all-targets` — **0 warnings in dsp-bench** (first
    pass surfaced 9 — 8 `doc_markdown` "InfluxDB"/"QuestDB" missing-backticks + 1
    `sort_by_key`; all fixed directly in source, no `#![allow]` added). 4
    pre-existing `splimes` warnings in untouched files remain.
  - `cargo fmt -p dsp-bench --check` — clean.
- **Post-merge re-verification (after merging origin/main with PR #7):**
  - `cargo test -p dsp-bench` — **36 passed, 0 failed** (34 from this PR + 2 from
    PR #7's timing-span tests, now combined on the merged tree).
  - `cargo clippy -p dsp-bench --all-targets` — **0 warnings in dsp-bench**.
  - `cargo fmt -p dsp-bench --check` — clean.
- **Done vs open:** DONE — a correct, tested, dependency-free ILP **format parser**
  (`parse`/`parse_points`, full escaping/typing/precision), wired + re-exported,
  docs + roadmap status; merge with PR #7 resolved. OPEN — wire an ILP/TSBS dataset
  *through a workload profile* (load a `.lp`/TSBS file → `Vec<Point>` → run a
  profile); the Phase-2 server-side ILP ingest endpoint; DuckDB adapter; richer
  report formats; full hardware capture.
- **Next step:** add an ILP-backed dataset source to the profile layer — e.g.
  `InterpolationProfile::from_line_protocol(path, field, precision)` or a
  `DatasetSource::{Generated, LineProtocol}` enum — so a TSBS-format file can drive
  the existing interpolation workload end-to-end, then the DuckDB adapter for the
  first competitor baseline.
- **PR:** https://github.com/physics515/DSP/pull/8

---

## 2026-06-09 — DSP-Bench: ILP/TSBS dataset wired through a workload profile

- **Item:** Phase 1 / Track 1–2 — **DSP-Bench**; roadmap "Immediate next actions"
  #5 ("Implement InfluxDB Line Protocol ingest") and backlog **B-ilp**. This is the
  exact next step the prior run's handoff named: take the dependency-free ILP
  *format parser* (`parse_points`, landed in PR #8) and wire a `.lp`/TSBS payload
  through an actual workload profile so it drives the existing interpolation harness
  end-to-end. Pure Rust, isolated to the `dsp-bench` leaf crate, no new deps. Keeps
  #5 / B-ilp at 🟡 — the profile-wiring sub-item is now done; the Phase-2 *server*
  ILP ingest endpoint remains the open work for that line.
- **What changed (isolated to `dsp-bench`, no new dependencies):**
  - `src/profile.rs` —
    - **new `DatasetSource` enum** (`Generated` | `LineProtocol { points }`): where a
      profile's input series comes from. The flagship stays `Generated` (seeded
      synthetic); `LineProtocol` carries a fixed, sorted, parsed series.
    - **new `InterpolationProfile::from_line_protocol(name, payload, field,
      precision, spline, resolution)`** — parses the payload via
      `line_protocol::parse_points`, projects the chosen numeric field onto sorted
      `Point`s, and returns a profile that drives the *same* `run_profile` harness.
      Records `seed=0`, `missingness=0.0` (data taken as given) and `span` = the
      data's actual extent.
    - **new `LineProtocolProfileError`** (`Parse(ParseError)` | `TooFewPoints(usize)`
      | `ZeroSpan`) with `Display` + `Error` (+ `source()`) + `From<ParseError>`:
      a spline needs ≥2 points spanning a non-zero range, so a one-point or
      single-instant series is rejected with a typed error rather than a panic or a
      bogus empty grid.
    - **`start()`/`end()` made source-aware**: a `LineProtocol` profile derives its
      time bounds from the data's own first/last timestamp (the synthetic epoch
      anchor is only used for `Generated`); factored the anchor into a private
      associated `anchor()` fn.
    - **`generate()` made source-aware**: returns the seeded synthetic series for
      `Generated`, a clone of the parsed series for `LineProtocol`. The old body was
      renamed `generate_synthetic()`. `run_profile` is unchanged — it calls
      `generate()`/`start()`/`end()` exactly as before, so the whole pipeline
      (latency stats, bootstrap CIs, timing spans, correctness gate, JSON report)
      now works on ILP-sourced data with zero harness changes.
  - `src/lib.rs` — re-export `DatasetSource`, `LineProtocolProfileError`
    (alongside `InterpolationProfile`); updated the `profile` module crate-doc
    bullet to mention the ILP source; added the end-to-end integration test.
  - `ROADMAP.md` — Immediate next action #5 note updated (parser → now wired
    end-to-end through a workload profile; stays 🟡, server endpoint still open).
- **Build/test/clippy (real, nightly `rustc 1.96.0-nightly` (55e86c996 2026-04-02)):**
  - `cargo build --workspace` — **GREEN** baseline confirmed before edits (38.98s).
  - `cargo test -p dsp-bench` — **42 passed, 0 failed, 0 ignored** (was 36; **+6
    new**: `from_line_protocol_parses_sorts_and_bounds_from_data`,
    `from_line_protocol_skips_records_missing_the_field`,
    `from_line_protocol_rejects_too_few_points`,
    `from_line_protocol_rejects_zero_span`,
    `from_line_protocol_surfaces_parse_errors`, and the lib-level
    `dsp_adapter_runs_a_line_protocol_sourced_profile` — a full `run_profile` over
    an 8-record TSBS-style payload that asserts the correctness gate passes and the
    result is publishable). 0 doc-tests.
  - `cargo clippy -p dsp-bench --all-targets` — **0 warnings in dsp-bench** (first
    pass surfaced 2 — `derive_partial_eq_without_eq` on `DatasetSource` and
    `unused_self` on `anchor`; both fixed directly: added `Eq` to the derive since
    `splimes::Point` is `Eq`, made `anchor` an associated fn. No `#![allow]` added.)
    4 pre-existing `splimes` warnings in untouched files remain (logged before).
  - `cargo fmt -p dsp-bench --check` — clean (exit 0).
  - Scope note: tests scoped to `-p dsp-bench` because the change is confined to the
    `dsp-bench` leaf crate (no other workspace member depends on it); the full
    workspace **build** is green, so no other crate could be affected. Did not run
    the full (heavy wgpu/GPU) `cargo test --workspace` to stay in the time budget.
- **Done vs open:** DONE — ILP/TSBS payloads now drive the real interpolation
  workload end-to-end via `DatasetSource::LineProtocol` + `from_line_protocol`,
  with typed-error guards, data-derived time bounds, and full test coverage incl. a
  `run_profile` integration test. OPEN — the Phase-2 server-side ILP ingest
  **endpoint**; loading a `.lp` file from disk in a runnable bench binary/CLI (the
  wiring exists at the library level; there's no `main` yet that reads a path); the
  long-deferred DuckDB adapter; richer report formats; full hardware capture.
- **Next step:** either (a) add a tiny bench runner/CLI entry point that reads a
  `.lp` file path + field/precision and emits a `BenchReport` to `reports/json/`
  (makes the ILP path runnable, not just library-testable), or (b) start the
  Phase-2 `axum` server with the ILP ingest endpoint. (a) is the smaller,
  budget-friendly increment and the natural close of the ILP-ingest line.
- **PR:** (to be filled in after `gh pr create`)
