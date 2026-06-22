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
- **PR:** https://github.com/physics515/DSP/pull/9

---

## 2026-06-10 — DSP-Bench: runnable CLI for line-protocol/TSBS benchmark runs

- **Item:** Phase 1 / Track 1–2 — **DSP-Bench**; roadmap "Immediate next actions"
  #5 ("Implement InfluxDB Line Protocol ingest") and backlog **B-ilp**. This is
  exactly option (a) the prior run's handoff named: add a small bench runner/CLI
  that reads a `.lp` file + field/precision and emits a `BenchReport` to
  `reports/json/`, so the ILP→interpolation path is *runnable from disk*, not just
  library-testable. Pure Rust, isolated to the `dsp-bench` leaf crate, no new deps
  (hand-rolled arg parsing, no `clap`). Keeps #5 / B-ilp at 🟡 — the file/CLI
  ingest path is now done; the Phase-2 *server* ILP ingest endpoint remains open.
- **What changed (isolated to `dsp-bench`, no new dependencies):**
  - **new `src/main.rs`** (binary `dsp-bench`) —
    - `Cli::from_args` — a pure, unit-tested arg parser accepting `--key value`
      and `--key=value` plus short flags (`-i`/`-f`); `--input` and `--field`
      required, everything else defaulted. Returns a `Command::{Help, Run}` enum so
      `-h`/`--help` short-circuits cleanly. A bare positional is taken as the input
      path; a second positional or unknown flag is a typed error.
    - value parsers `parse_precision` (ns|us|ms|s + long forms), `parse_spline`
      (linear|quadratic|cubic|`poly[:N]` → `Spline::Polynomial(N, None)`),
      `parse_resolution` (ns|us|ms|s|m|h|d|w|mo|y), `parse_reps` (rejects 0, which
      `run_profile` requires).
    - `run` — reads the file, builds the profile via
      `InterpolationProfile::from_line_protocol`, runs the `DspAdapter` through
      `run_profile`, wraps the result in a `BenchReport` with real wall-clock
      `RunMetadata::capture(Utc::now().to_rfc3339())`, and writes
      `<out-dir>/<profile>__dsp.json` (default `reports/json/`). Profile name
      defaults to the input file stem.
    - exit codes: `0` on a publishable (correctness-passing) run, `1` on a parse
      error / missing file / failed correctness gate — so a CI/scripted caller can
      gate on it. A current-thread tokio runtime keeps the binary lean.
    - `print_summary` — concise human summary (profile/adapter/reps/in–out points,
      p50/p95/p99/mean latency in ms, throughput, correctness, publishable, report
      path).
  - `README.md` — new bullets for the end-to-end-wired ILP profile source and the
    CLI runner; corrected the stale "Not yet" line (the ILP-through-profile item is
    done — the open ILP work is the Phase-2 server endpoint); added a `cargo run`
    usage example.
  - `ROADMAP.md` — Immediate next action #5 note updated ("Now runnable from
    disk"); stays 🟡 (server endpoint still open).
- **Build/test/clippy (real, nightly `rustc 1.98.0-nightly` (cb46fbb8c 2026-06-08)):**
  - `cargo build --workspace` — **GREEN** baseline confirmed before edits
    (2m43s; heavy turso/wgpu deps).
  - `cargo build -p dsp-bench --bins` — **GREEN** (binary compiles; first pass
    failed on a `derive(Eq)` for `Cli`/`Command` because `Spline::Polynomial`
    carries an `f64` — fixed by dropping to `PartialEq`, tests use `assert_eq!`).
  - `cargo test -p dsp-bench` — **53 passed, 0 failed, 0 ignored** (42 lib, was 42;
    **+11 new** in `main.rs`: arg-parsing defaults, short/positional/`=`-forms,
    every precision/spline(incl. polynomial)/resolution token, `--help`
    short-circuit, missing-required errors, unknown-flag/zero-reps/dangling-value
    rejection, second-positional rejection, and `derive_profile_name`). 0
    doc-tests.
  - `cargo clippy -p dsp-bench --all-targets` — **0 warnings in dsp-bench** (first
    pass surfaced 2 — `clippy::use_self` on the `Self`-return and a needless
    `.peekable()`; both fixed directly, no `#![allow]`). The 4 pre-existing
    `splimes` `sort_by_key` warnings in untouched files remain (logged before).
  - `cargo fmt -p dsp-bench --check` — clean (exit 0).
  - **Real end-to-end run** (not just tests): built an 8-record TSBS-style
    `sample.lp` in the per-run scratch dir and ran
    `dsp-bench --input sample.lp --field usage --precision s --spline cubic
    --resolution minutes --reps 12 --out-dir <scratch>` — exit 0, correctness
    PASS, wrote a valid **schema v3** JSON report (8 input → 11 output points,
    `metadata.os=windows`, real RFC-3339 `generated_at`). Error paths verified:
    `--help`→0, missing `--field`→1, nonexistent file→1, unknown flag→1.
  - Scope note: tests scoped to `-p dsp-bench` (the change is confined to the
    `dsp-bench` leaf crate; no other workspace member depends on it, and the full
    workspace **build** is green). Did not run the full (heavy wgpu/GPU)
    `cargo test --workspace` to stay in the time budget.
- **Done vs open:** DONE — the ILP→interpolation path is now runnable from disk
  via the `dsp-bench` binary, with a tested arg parser, typed value parsing,
  correctness-gated exit codes, JSON artifact output, and a verified end-to-end
  run. OPEN — the Phase-2 server-side ILP ingest **endpoint** (`axum`); the
  long-deferred DuckDB adapter (first competitor baseline); richer report formats
  (Parquet/HTML); full hardware capture in run metadata; the methodology document.
- **Next step:** either (a) the **DuckDB adapter** — the first competitor baseline,
  so reports compare DSP against a real CPU-only engine (roadmap Immediate Next
  Action #3, long deferred), or (b) start the **Phase-2 `axum` server** with the
  ILP ingest endpoint. (a) is the higher-leverage benchmark increment now that the
  DSP-side ingest/run/report loop is complete end-to-end.
- **PR:** https://github.com/physics515/DSP/pull/10

---

## 2026-06-11 — DSP-Bench: portable linear baseline adapter + `--compare`

- **Item:** Phase 1 / Track 1 — **DSP-Bench**; roadmap "Immediate next actions"
  #3 ("Add DSP and DuckDB adapters") and fair-protocol **Phase 1.2 class (C)**
  ("portable client-side baseline: fetch raw → interpolate in the same Rust
  client → total end-to-end"). Until tonight DSP-Bench had exactly one system
  (`DspAdapter`), so it could not emit a *comparison* at all. This adds the first
  non-DSP system — a dependency-free, in-process linear baseline — turning the
  harness from "measure DSP" into "compare DSP against a reference". Chose this
  over the long-deferred DuckDB adapter the prior handoff floated because DuckDB
  needs a native library + the `duckdb` crate (heavy/fragile build on Windows,
  risks the nightly time budget); the portable baseline is the honest reference
  the roadmap explicitly asks for and lands clean with zero new deps. Keeps #3 at
  🟡 (the external-engine DuckDB baseline is still open).
- **What changed (isolated to `dsp-bench`, no new dependencies):**
  - **new `src/baseline_adapter.rs`** (`BaselineLinearAdapter`) — implements the
    vendor-neutral `SystemAdapter` trait with piecewise-linear reconstruction:
    sorts the (possibly out-of-order/ILP-sourced) input in place, builds the same
    `generate_target_times(start,end,resolution)` grid the harness expects (so
    output counts match DSP and the correctness gate passes identically), and
    interpolates each grid point from its bracketing pair. **Precision-aware:** the
    whole interpolation runs in `BigDecimal` (time ratio is an exact `i64`-ns
    rational), so it never silently downcasts through `f64` — honouring the same
    hard constraint as the engine. Out-of-range grid points extrapolate along the
    nearest end segment; a single input point degenerates to a constant;
    zero/unrepresentable spans degenerate to the left value (no divide-by-zero);
    empty input is a typed error. The adapter is **always linear by definition**
    (ignores the requested `Spline`) and named `baseline-linear`, documented
    plainly so the comparison is an honest quality/speed reference, not a disguised
    apples-to-apples spline race. 7 unit tests (regular grid, irregular midpoint,
    out-of-order sort, linear extrapolation past both ends, single-point constant,
    empty-input rejection, spline-ignored-stays-linear).
  - `src/lib.rs` — `pub mod baseline_adapter;`, re-export `BaselineLinearAdapter`,
    crate-doc bullet.
  - `src/main.rs` — new `--compare`/`-c` flag (`Cli.compare`): runs DSP and, when
    set, the baseline, collecting both into one `BenchReport`. Artifact filename
    now tags every adapter (`<profile>__dsp+baseline-linear.json`, sanitized to
    `...dsp-baseline-linear.json`) so a comparison and a solo run never collide;
    `print_summary` now prints one block per adapter; USAGE updated. +1 CLI test
    (`compare_flag_is_parsed_in_both_forms`) and a `compare`-default assertion.
  - `dsp-bench/README.md` — baseline-adapter bullet, `--compare` usage example,
    reworded the "Not yet" line (the baseline is the first non-DSP system; the
    external-engine DuckDB adapter remains open).
  - `ROADMAP.md` — Immediate next action #3 note updated (baseline landed; stays
    🟡, external DuckDB adapter still open).
- **Build/test/clippy (real, nightly `rustc 1.98.0-nightly` (cb46fbb8c 2026-06-08)):**
  - `cargo build --workspace` — **GREEN** baseline confirmed before edits (1m22s;
    heavy turso/wgpu deps).
  - `cargo test -p dsp-bench` — **61 passed, 0 failed, 0 ignored** (49 lib, was 42,
    **+7** baseline-adapter tests; 12 bin, was 11, **+1** compare-flag test). 0
    doc-tests.
  - `cargo clippy -p dsp-bench --all-targets` — **0 warnings in dsp-bench** (first
    pass surfaced 2 `clippy::cast_precision_loss` from `i as f64` in my new tests;
    fixed directly with an `idx()` helper using `f64::from(u16::try_from(..))`, no
    `#[allow]`). The 4 pre-existing `splimes` `sort_by_key` warnings in untouched
    files remain (logged before).
  - `cargo fmt -p dsp-bench --check` — clean (exit 0).
  - **Real end-to-end `--compare` run** (not just tests): built an 8-record
    TSBS-style `sample.lp` in the per-run scratch dir and ran
    `dsp-bench --input sample.lp --field usage --precision s --spline cubic
    --resolution minutes --reps 12 --out-dir <scratch> --compare` — exit 0, **both**
    adapters PASS correctness, both produce 11 output points from 8 inputs, report
    publishable, wrote a valid schema-v3 `sample__dsp-baseline-linear.json` with
    both `"adapter": "dsp"` and `"adapter": "baseline-linear"`. Honest datapoint:
    on this tiny dataset DSP's cubic GPU/SIMD path carries real fixed setup cost
    (~498 ms mean) while the naive in-process linear baseline is ~0.014 ms — the
    benchmark surfacing DSP's small-input overhead rather than hiding it. Error
    paths reverified: `--help`→0, missing `--field`→1, nonexistent file→1.
  - Scope note: tests scoped to `-p dsp-bench` (the change is confined to the
    `dsp-bench` leaf crate; no other workspace member depends on it, and the full
    workspace **build** is green). Did not run the full (heavy wgpu/GPU)
    `cargo test --workspace` to stay in the time budget.
- **Done vs open:** DONE — DSP-Bench now has a second, vendor-neutral, in-process
  system and can emit a real two-system comparison report end-to-end (library +
  CLI + verified run). OPEN — the external-engine **DuckDB adapter** (a real
  database baseline); ClickHouse/InfluxDB 3/QuestDB/TimescaleDB adapters; the
  Phase-2 server-side ILP ingest **endpoint** (`axum`); richer report formats
  (Parquet/HTML); full hardware capture; the methodology document.
- **Next step:** either (a) the **DuckDB adapter** — the first *real-database*
  competitor baseline (needs the `duckdb` crate + bundled lib; do it as its own
  out-of-core adapter module, budget permitting), or (b) start the **Phase-2
  `axum` server** with the ILP ingest endpoint. With the comparison plumbing now
  proven, (a) is the higher-leverage benchmark increment.
- **PR:** https://github.com/physics515/DSP/pull/11

---

## 2026-06-12 — DSP-Bench: portable forward-fill (LOCF) baseline + 3-system `--compare`

- **Item:** Phase 1 / Track 1 — **DSP-Bench**; roadmap "Immediate next actions"
  #3 ("Add DSP and DuckDB adapters") and fair-protocol **Phase 1.2 class (B)**
  ("native in-DB gap-fill, reproduced portably"). Until tonight DSP-Bench had
  exactly two systems — DSP and the in-process linear baseline (class C). The
  single most common reconstruction real time-series engines actually ship is
  *last-observation-carried-forward* (LOCF): InfluxDB `FILL(previous)`, QuestDB
  `FILL(prev)`, TimescaleDB `locf()`. This adds that as a third portable,
  dependency-free, in-process system so a comparison report can carry DSP's
  interpolation against the gap-fill databases really use — not only linear.
  Chose this over the long-deferred external DuckDB adapter (needs the `duckdb`
  crate + a bundled native lib; heavy/fragile build on Windows, risks the nightly
  time budget) because the portable LOCF baseline is the honest class-(B)
  reference the roadmap explicitly asks for and lands clean with zero new deps.
  Keeps #3 at 🟡 (the external-engine DuckDB baseline is still open).
- **What changed (isolated to `dsp-bench`, no new dependencies):**
  - **new `src/forward_fill_adapter.rs`** (`ForwardFillAdapter`, name
    `baseline-forward-fill`) — implements the vendor-neutral `SystemAdapter` trait
    with piecewise-constant LOCF reconstruction: sorts the (possibly
    out-of-order/ILP-sourced) input in place, builds the same
    `generate_target_times(start,end,resolution)` grid the harness expects (so
    output counts match DSP and the correctness gate passes identically), and for
    each grid point holds the value of the latest sample whose timestamp is `<= t`
    (binary search). **No fabricated precision:** a held value is the prior
    sample's exact `BigDecimal`, copied verbatim — never arithmetic — so it cannot
    drift through `f64` and every value stays finite. Boundary convention: before
    the first sample (no prior observation) it holds the *first* value backward
    rather than emitting a null the correctness gate forbids; after the last sample
    it holds flat (no extrapolated slope, unlike the linear baseline). Empty input
    is a typed error; a single point degenerates to a constant. Always forward-fill
    by definition (ignores the requested `Spline`). 7 unit tests (holds-between-
    samples step, carry-last-forward, hold-first-backward, out-of-order sort,
    single-point constant, empty-input rejection, spline-ignored-stays-a-step).
  - `src/lib.rs` — `pub mod forward_fill_adapter;`, re-export `ForwardFillAdapter`,
    crate-doc bullet.
  - `src/main.rs` — `--compare` now runs the full portable-baseline suite (linear
    **and** forward-fill), collecting all three into one `BenchReport`; doc comment
    + USAGE text updated. The artifact tag already joins every adapter name, so the
    filename auto-extends to `<profile>__dsp+baseline-linear+baseline-forward-fill.json`.
  - `dsp-bench/README.md` — forward-fill-adapter bullet, reworded the "Not yet"
    line (two portable baselines now), updated the `--compare` usage example.
  - `ROADMAP.md` — Immediate next action #3 note updated (forward-fill landed;
    three-system comparison; stays 🟡, external DuckDB adapter still open).
- **Build/test/clippy (real, nightly `rustc 1.98.0-nightly` (cb46fbb8c 2026-06-08)):**
  - `cargo build --workspace` — **GREEN** baseline confirmed before edits (2m00s;
    heavy turso/wgpu deps).
  - `cargo test -p dsp-bench` — **68 passed, 0 failed, 0 ignored** (56 lib, was 49,
    **+7** forward-fill-adapter tests; 12 bin, unchanged). 0 doc-tests.
  - `cargo clippy -p dsp-bench --all-targets` — **0 warnings in dsp-bench** (first
    pass surfaced 3 `clippy::doc_markdown` "missing backticks" on `InfluxDB`/
    `QuestDB`/`TimescaleDB` in my new module doc; fixed directly by backticking
    them, no `#[allow]`). The 4 pre-existing `splimes` `sort_by_key` warnings in
    untouched files remain (logged before).
  - `cargo fmt -p dsp-bench --check` — clean (exit 0).
  - **Real end-to-end `--compare` run** (not just tests): built an 8-record
    TSBS-style `sample.lp` in the per-run scratch dir and ran
    `dsp-bench --input sample.lp --field usage --precision s --spline cubic
    --resolution minutes --reps 12 --out-dir <scratch> --compare` — exit 0, **all
    three** adapters PASS correctness, each produces 10 output points from 8 inputs,
    report publishable, wrote a valid schema-v3
    `sample__dsp-baseline-linear-baseline-forward-fill.json` with all three
    `"adapter"` entries (`dsp`, `baseline-linear`, `baseline-forward-fill`), every
    `values_finite=true`. Honest datapoint on this tiny dataset: forward-fill is
    the fastest (~0.004 ms mean, pure copies), linear next (~0.010 ms), DSP's cubic
    GPU/SIMD path carries real fixed setup cost (~505 ms mean) — the benchmark
    surfacing DSP's small-input overhead rather than hiding it. Error paths
    reverified: `--help`->0, missing `--field`->1, nonexistent file->1.
  - Scope note: tests scoped to `-p dsp-bench` (the change is confined to the
    `dsp-bench` leaf crate; no other workspace member depends on it, and the full
    workspace **build** is green). Did not run the full (heavy wgpu/GPU)
    `cargo test --workspace` to stay in the time budget.
- **Done vs open:** DONE — DSP-Bench now has a third, vendor-neutral, in-process
  system (LOCF) and can emit a real three-system comparison report end-to-end
  (library + CLI + verified run). OPEN — the external-engine **DuckDB adapter** (a
  real database baseline); ClickHouse/InfluxDB 3/QuestDB/TimescaleDB adapters; the
  Phase-2 server-side ILP ingest **endpoint** (`axum`); richer report formats
  (Parquet/HTML); full hardware capture; the methodology document.
- **Next step:** either (a) the **DuckDB adapter** — the first *real-database*
  competitor baseline (needs the `duckdb` crate + bundled lib; do it as its own
  out-of-core adapter module, budget permitting), or (b) an **accuracy-metrics
  module** (RMSE/MAE/max-error/bias over a known synthetic ground truth) so the
  three reconstruction methods can be compared on *quality*, not only speed —
  which the roadmap repeatedly calls for (Phase 1.2 correctness, Phase 6.4). With
  three reconstruction methods now in place, (b) is the natural higher-leverage
  increment; (a) remains the path to a real-engine comparison.
- **PR:** https://github.com/physics515/DSP/pull/12


---

## 2026-06-13 — DSP-Bench: reconstruction-accuracy / quality axis (6 increments)

A focused night building DSP-Bench's **quality** axis end-to-end, all in the
`dsp-bench` leaf crate. Until tonight the harness measured only speed; it now
scores how *right* each reconstruction is against a known analytic ground truth,
and can emit a real DSP-vs-baselines quality comparison from the command line.

**Item:** Phase 1 / Track 1 — DSP-Bench; roadmap "Immediate next actions" #3
(now records the quality axis) and fair-protocol **Phase 1.2** ("benchmark
interpolation quality as well as speed") + **Phase 6.4** (RMSE/MAE/max-error/bias).
Picked the previous run's recommended option (b) — the accuracy-metrics module —
over the long-deferred external DuckDB adapter (heavy/fragile native build on
Windows, risks the budget); it lands clean with zero new dependencies and the
roadmap repeatedly asks for it.

**Increment 1 — `accuracy.rs` (new) + ground-truth plumbing** (commit `7157312`)
- `profile.rs`: factored the synthetic clean signal into a shared
  `clean_signal(phase)` so generation and ground truth can never drift; exposed
  `clean_signal_at(t) -> Option<f64>` (Some for Generated, None for line-protocol).
- `accuracy.rs`: `AccuracyMetrics { count, rmse, mae, max_abs_error, bias }` via
  `from_aligned(predicted, truth)`; `synthetic_ground_truth(profile)` evaluates
  truth on the same `generate_target_times` grid a passing adapter produces.
  Typed `AccuracyError` for length-mismatch / empty / non-finite / no-ground-truth.
- `lib.rs`: `measure_accuracy(adapter, profile)` runner + re-exports.
- Tests: +13 (2 profile, 9 accuracy, 2 lib). Build green; clippy fixed directly
  (mul_add, doc-paragraph split, i64::try_from) — no `#[allow]`.

**Increment 2 — accuracy in `BenchResult` (schema v4)** (commit `d87cc2f`)
- `schema.rs`: `BenchResult.accuracy: Option<AccuracyMetrics>`, SCHEMA_VERSION 4,
  `#[serde(default, skip_serializing_if = "Option::is_none")]` (pre-v4 artifacts
  still parse; absent accuracy omitted, not `null`).
- `lib.rs`: `run_profile` scores the final rep's output against ground truth when
  available (reuses `last_output`, no extra adapter run); absent for line-protocol.
- `main.rs`: summary prints an accuracy line when present. `report.rs` fixture updated.
- Tests: +1 (75→... cumulative). f64 round-trip compared with tolerance (ULP drift).

**Increment 3 — `--synthetic` CLI mode** (commit `8448998`)
- `profile.rs`: public `InterpolationProfile::synthetic(name, params)` constructor;
  flagship delegates to it. `DEFAULT_SEED` made pub.
- `main.rs`: `--synthetic`/`-s` runs the generator (the only mode with a ground
  truth, so the only one reporting accuracy); `--seed` (dec/0x-hex), `--points`
  (>=2), `--missingness`/`--jitter` ([0,1]). `input`/`field` now Option, validated
  per mode (synthetic rejects an input file / `--field` as a conflict).
- Verified real run: first quality comparison artifact. Honest anti-Goodhart
  result on the noisy flagship signal — linear best RMSE (1.19), DSP cubic
  overshoots noise (1.91), forward-fill worst (2.93). +4 bin tests.

**Increment 4 — configurable noise amplitude + `SyntheticParams`** (commit `7d06ced`)
- `profile.rs`: `noise_amplitude` knob (`0` = samples exactly on ground truth);
  introduced `SyntheticParams` (7 knobs, `Default` = flagship) and reduced
  `synthetic` to `(name, params)` — this structurally fixed the `too_many_arguments`
  clippy lint the 8th arg would trip (no `#[allow]`), then derived `Copy` to clear
  `needless_pass_by_value`.
- `main.rs`: `--noise <F>` (finite, >=0). `lib.rs`: re-export `SyntheticParams`.
- Tests: +3 (zero-noise-on-truth; noise>1 accepted; clean reconstructs strictly
  better than noisy). Honest finding: `--noise 0` cuts DSP cubic RMSE 1.9→0.87 but
  linear still leads on this high-frequency signal (cubic overshoots near gaps).

**Increment 5 — docs** (commit `c61ec85`)
- `dsp-bench/README.md`: accuracy + two-input-mode bullets, a "Quality comparison
  (synthetic mode)" run section with the linear-beats-cubic honesty note.
- `ROADMAP.md`: Immediate-next-action #3 records the accuracy module, schema-v4
  field, `--synthetic` mode + knobs, and the first anti-Goodhart result. Stays 🟡.

**Increment 6 — `BenchReport::most_accurate()` + CLI winner line** (commit `047be53`)
- `report.rs`: returns the publishable result with the smallest finite RMSE among
  those carrying accuracy; skips no-accuracy / failing / NaN results. +3 tests.
- `main.rs`: prints `most accurate: <adapter> (rmse=..)` for synthetic compare runs.

**Build/test/clippy (real, nightly):**
- `cargo build --workspace` — GREEN (confirmed at baseline and again at end).
- `cargo test -p dsp-bench` — **75 lib + 17 bin + 0 doc, all pass** (was 56 lib +
  12 bin; +19 lib, +5 bin this night).
- `cargo clippy -p dsp-bench --all-targets` — **0 dsp-bench warnings** (every lint
  introduced was fixed structurally, never with `#[allow]`). The 4 pre-existing
  `splimes` `sort_by_key` warnings in untouched files remain (logged before).
- `cargo fmt -p dsp-bench --check` — clean.
- Real end-to-end runs verified per increment (line-protocol `--compare`, synthetic
  `--compare`, `--noise 0`); all exit 0 with passing correctness gates.
- Scope note: tests scoped to `-p dsp-bench` — the change is confined to the
  `dsp-bench` leaf crate (no other workspace member depends on it) and the full
  workspace **build** is green. Did not run the full (heavy wgpu/GPU/turso)
  `cargo test --workspace` to stay in the time budget.

**Done vs open:** DONE — DSP-Bench now has a complete quality axis: accuracy
metrics module, accuracy carried in every synthetic result (schema v4), a
`--synthetic` CLI mode with seed/points/missingness/jitter/noise knobs, and a
report-level "most accurate" query, all surfaced in the CLI. OPEN — the
external-engine **DuckDB adapter** (a real database baseline);
ClickHouse/InfluxDB 3/QuestDB/TimescaleDB adapters; the Phase-2 server-side ILP
ingest **endpoint** (`axum`); richer report formats (Parquet/HTML); full hardware
capture; the standalone methodology document (#10); signal-shape variety in the
generator.

**STOP REASON:** completed a coherent feature arc (the whole accuracy/quality axis
end-to-end across 6 increments, well above the 2–4 bar). The next substantive
roadmap items — the external DuckDB adapter and the axum ILP-ingest server — are
too large/fragile to land as another clean bounded slice in the remaining window
(landing one half-built would leave the PR unfocused); stopping here keeps the PR
reviewable and the workspace green.

**Next step (tomorrow):** start the **DuckDB external-engine adapter** as its own
out-of-core module (the first *real-database* baseline; needs the `duckdb` crate
+ bundled native lib — budget a clean Windows build) OR begin the **Phase-2
`axum` server** with the ILP ingest endpoint. With the quality axis now proven,
either advances the benchmark toward real competitor comparisons.

**PR:** https://github.com/physics515/DSP/pull/13


---

## 2026-06-14 — DSP-Bench: signal-shape variety + HTML report + hardware capture (7 increments)

A night extending DSP-Bench's quality/reporting axes, all in the `dsp-bench` leaf
crate. The synthetic generator could only ever produce one signal shape, reports
were JSON-only, and run metadata carried no hardware — three open items the prior
log flagged. All three are now closed end-to-end.

**Item:** Phase 1 / Track 1 — DSP-Bench; roadmap "Immediate next actions" #3
(records the shape-selectable ground truth) and the benchmark-report template's
hardware block. Picked these clean, no-large-dependency slices over the two big
deferred items (external DuckDB native build · Phase-2 `axum` server) which the
prior two runs flagged as too large/fragile to land as a bounded late-night slice.

**Increment 1 — `SignalShape` enum** (commit `41e72ca`)
- `profile.rs`: `SignalShape { MultiSine, Sawtooth, Step, DampedSine }` +
  `evaluate(phase)`, each a deterministic, finite, `[10,90]`-bounded curve;
  `signal_shape` field on `SyntheticParams` (Default = MultiSine) and
  `InterpolationProfile`. Both generation and the analytic ground truth
  (`clean_signal_at`) route through the selected shape so they can never drift.
  Removed the private `clean_signal` fn. lib.rs/main.rs re-export; CLI kept the
  default for now. Tests: +4 (range-bounded for all shapes; default is MultiSine;
  step is piecewise-constant; changing shape changes the truth).

**Increment 2 — `--shape` CLI flag** (commit `1375436`)
- `main.rs`: `--shape multisine|sawtooth|step|dampedsine` (case/separator-
  insensitive, short aliases) feeds `SyntheticParams.signal_shape`; inert in
  line-protocol mode (no ground truth), matching the other synthetic knobs. Help
  + 2 bin tests. Verified end-to-end: on `sawtooth` the linear baseline
  out-accuracies DSP's cubic (overshoots the discontinuities) — honest, surfaced.

**Increment 3 — record `signal_shape` in the artifact (schema v5)** (commit `b3effa8`)
- The `--shape` knob had opened a reproducibility gap. `profile.rs`: derive
  serde on `SignalShape` (lowercase tokens) + `ground_truth_shape()` (Some for
  generated, None for line-protocol). `schema.rs`: `DatasetMeta.signal_shape:
  Option<SignalShape>`, SCHEMA_VERSION 4→5, serde default+skip. `lib.rs` records
  it; `main.rs` prints it. Tests: +4 (v4 artifact still parses; round-trips
  lowercase; omitted when absent; ground_truth_shape Some/None).

**Increment 5 — end-to-end shape-sweep test** (commit `86e6879`)
- `lib.rs`: drives `run_profile` through every shape, asserting the artifact
  records the shape, correctness passes, and accuracy is present + finite. +1.

**Increment 4 — docs** (commit `04e680b`)
- `dsp-bench/README.md` + `ROADMAP.md` #3: record the shape-selectable ground
  truth, `--shape`, schema-v5 `dataset.signal_shape`, and the per-shape honesty
  finding (cubic leads on multisine/step, linear wins on sawtooth). (Committed
  before increment 5; listed here in feature order.)

**Increment 6 — self-contained HTML report (`--html`)** (commit `e7047fb`)
- `report.rs`: `BenchReport::to_html()` renders a full HTML document (inline CSS,
  no external assets/scripts) — one table row per system (shape, latency
  percentiles, throughput, correctness, accuracy), most-accurate row highlighted;
  all caller strings `escape_html`'d. `write_html()` + `default_html_filename`
  mirror the JSON path. `main.rs`: `--html` writes it beside the JSON. README
  updated. Tests: +5 lib +1 bin (filename mirror; document structure; exactly-one
  best row / none without accuracy; HTML-escaping; disk round-trip; flag parse).

**Increment 7 — CPU/RAM hardware capture in run metadata** (commit `3a18f42`)
- `report.rs`: `RunMetadata` gains `cpu_model`, `cpu_cores_physical`,
  `cpu_cores_logical`, `total_memory_bytes` (all Option, serde default+skip).
  `capture()` probes via `sysinfo` (already a workspace dep — no new lockfile
  packages) using a *targeted* refresh (`new()` + `refresh_memory` +
  `refresh_cpu_all`), NOT `new_all()`: `new_all` enumerates every process and
  OOM'd the test run on this host (a 16 GiB allocation) — the lighter probe is
  also more correct for CPU+memory-only needs. HTML renders a hardware meta line;
  the CLI summary prints the CPU model. Cargo.toml + Cargo.lock (sysinfo edge).
  Tests: +2 (probe self-consistency: positive counts, logical >= physical, a host
  reports cores+RAM; HTML surfaces the hardware).

**Build/test/clippy (real, nightly `rustc 1.98.0-nightly`):**
- `cargo build --workspace` — GREEN (confirmed at baseline and again at end).
- `cargo test -p dsp-bench` — **91 lib + 20 bin + 0 doc, all pass** (was 75 lib +
  17 bin at the start of the night; +16 lib, +3 bin).
- `cargo clippy -p dsp-bench --all-targets` — **0 dsp-bench warnings** under
  `#![warn(clippy::pedantic, clippy::nursery, clippy::all)]`; every lint
  introduced (trailing-comma, cast, etc.) was fixed structurally, never with
  `#[allow]` beyond the file's existing cast allows. The 4 pre-existing `splimes`
  `sort_by_key` warnings in untouched files remain (logged before).
- `cargo fmt -p dsp-bench --check` — clean.
- Real end-to-end runs verified per increment (`--shape step`/`sawtooth`,
  `--html`, hardware capture); all exit 0 with passing correctness gates.
- Scope note: tests scoped to `-p dsp-bench` — the change is confined to the
  `dsp-bench` leaf crate (no other workspace member depends on it) and the full
  workspace **build** is green. Did not run the full (heavy wgpu/GPU/turso)
  `cargo test --workspace` to stay in the night's budget.

**Done vs open:** DONE — the synthetic generator is now shape-selectable across
four analytic curves, each recorded in the artifact (schema v5) for exact
regeneration and surfaced through `--shape`; reports render to a self-contained
HTML view (`--html`) beside the JSON; run metadata captures CPU model / core
counts / total RAM. OPEN — the external-engine **DuckDB adapter** (real-database
baseline); ClickHouse/InfluxDB 3/QuestDB/TimescaleDB adapters; the Phase-2
server-side ILP ingest **endpoint** (`axum`); the **Parquet** report format; the
rest of the hardware block (disk, GPU, driver versions); the standalone
methodology document (#10).

**STOP REASON:** no workable next slice that fits as another clean bounded
increment on this PR. The seven increments completed a coherent arc (shape
variety + HTML + hardware capture, well above the 2–4 bar); the remaining
top-priority roadmap items each require a large new dependency tree (`axum` →
hyper/tower, absent from the workspace) or a fragile native build (`duckdb`
bundled lib on Windows) — both deferred by the prior two runs — and starting one
half-built would pull two unrelated concerns onto this PR and risk the build.

**Next step (tomorrow):** start the **Phase-2 `axum` server** as its own new
workspace member (`dsp-server`), first slice = server skeleton + `/health` +
`/ready` returning JSON, compiles + 1 test, wired into the root `Cargo.toml`
(adds the axum dependency tree deliberately, as its own increment) — then the ILP
ingest endpoint next; OR the **DuckDB external-engine adapter** as its own
out-of-core module (budget a clean Windows bundled build). Either advances the
benchmark toward real competitor comparisons / a public API.

**PR:** https://github.com/physics515/DSP/pull/14

---

## 2026-06-15 — Phase 2 ignition: dsp-server (axum) + shared ILP crate (6 increments)

The night that started the **Phase-2 benchmark-grade server/API**, deferred by the
prior three runs as "too large for a late-night slice". It is pure-Rust (no
fragile native build), so it landed cleanly as a series of bounded increments: a
new `dsp-server` crate grew from a health/ready skeleton to a real interpolation
API (JSON + InfluxDB Line Protocol) with Prometheus metrics, and the ILP parser
was extracted into a shared `dsp-line-protocol` crate so the server and the bench
harness speak one dialect.

**Item:** roadmap Phase 2 (benchmark-grade server/API) + "Immediate next actions"
#5 (InfluxDB Line Protocol ingest). Followed the prior run's explicit "next step"
recommendation to start the axum server as its own workspace member.

**Increment 1 — `dsp-server` Phase-2 API skeleton** (commit `dc22eb4`)
- New workspace member `dsp-server` (axum 0.8 + tower dev-dep added to the
  workspace dep table). `app()` is the single source of truth for the route
  table; `GET /health` (liveness) and `GET /ready` (readiness) return JSON.
  Binary binds `127.0.0.1:8080`, overridable via `DSP_SERVER_ADDR`. No
  vendor-specific deps. Tests: 4 lib.

**Increment 2 — `POST /api/v1/interpolate`** (commit `c5d48b4`)
- First capability endpoint: drives `splimes::auto_interpolate` over HTTP+JSON.
  Vendor-neutral `SplineSpec` (linear/quadratic/cubic/polynomial) + `ResolutionSpec`
  map to the engine enums; range bounds default to the input span. `ApiError`
  renders `{"error": "..."}` (empty set / non-finite value / inverted range -> 400).
  Wire values are JSON f64, widened to BigDecimal before the engine (BigDecimal
  stays the logical type; documented). Verified live (linear 0->60 over 60s = 61
  points). Tests: +6 (10 lib total).

**Increment 3 — Prometheus `/metrics`** (commit `21f01ff`)
- Dependency-free metrics layer (renders Prometheus text v0.0.4 by hand).
  `Metrics` groups per-endpoint sub-counters behind `SharedMetrics = Arc<Metrics>`;
  the interpolate handler records request/error/output-point counts via a thin
  wrapper. `app_with_metrics` lets a test observe live counters. The
  `struct_field_names` clippy lint was fixed structurally (per-endpoint nesting),
  not suppressed. Tests: +4 (14 lib total).

**Increment 4 — extract shared `dsp-line-protocol` crate** (commit `c469c8d`)
- Moved the vendor-neutral ILP format parser out of `dsp-bench` into a new
  workspace member so the harness and the server share one parser. `dsp-bench`
  keeps a thin `line_protocol` re-export module — every existing
  `crate::line_protocol::*` path is unchanged. The 11 ILP tests moved with the
  code (dsp-bench lib 91->80, dsp-line-protocol +11; total preserved).

**Increment 5 — `POST /api/v1/interpolate/ilp`** (commit `1c25a96`)
- Server-side ILP ingest built on the shared crate: ILP payload as the
  `text/plain` body, field/precision/spline/resolution as query params, series
  range = the data's own span. Reuses the same `run_interpolation` core as the
  JSON endpoint (identical response envelope) and the same metrics counters.
  Robust token parsing gives clear 400s; rejects malformed payloads, <2 points,
  zero-span series. Verified live (2-row cpu payload -> 61-point grid; /metrics
  reflects it). Tests: +5 (19 lib total).

**Increment 6 — README + ROADMAP status** (commit `eb93615`)
- `dsp-server/README.md`: running the binary, every endpoint with curl examples,
  the numeric boundary, the Prometheus output. `ROADMAP.md`: marked Immediate
  Next Action #5 (ILP ingest) ✅ and added a Phase-2 "started" status note. No
  code touched.

**Build/test/clippy (real, nightly `rustc 1.98.0-nightly (cb46fbb8c 2026-06-08)`):**
- `cargo build --workspace` — GREEN (confirmed at baseline and at end).
- `cargo test -p dsp-server` — **19 lib pass**.
- `cargo test -p dsp-line-protocol` — **11 lib pass**.
- `cargo test -p dsp-bench` — **80 lib + 20 bin pass** (was 91 lib before the ILP
  parser moved out; 80 + 11 in the new crate = the same total, no test lost).
- `cargo clippy -p dsp-server -p dsp-line-protocol --all-targets` — **0 warnings**
  under `#![warn(clippy::pedantic, clippy::nursery, clippy::all)]`; every lint
  hit (double_must_use, missing-backticks, struct_field_names, too_long_first_doc,
  map().unwrap_or_else()) was fixed structurally, never with `#[allow]`. The 4
  pre-existing `splimes` `sort_by_key` warnings in untouched files remain.
- `cargo fmt` — clean for both new crates (hard tabs, max_width 10000).
- Live end-to-end runs verified per capability increment (health/ready,
  interpolate JSON, interpolate ILP, /metrics) against a bound server.
- Scope note: tests scoped to the touched crates; the change is confined to two
  new leaf crates + a re-export shim in `dsp-bench`, and the full workspace
  **build** is green. Did not run the full (heavy wgpu/GPU/turso) `cargo test
  --workspace` to stay in the night's budget.

**Done vs open:** DONE — Phase 2 is off the ground: a real `dsp-server` with
liveness/readiness, JSON interpolation over the `splimes` engine, server-side
InfluxDB-Line-Protocol ingest, and Prometheus metrics; the ILP parser is now a
shared vendor-neutral crate. OPEN (Phase 2 remainder) — DB/subject/aspect
management, schema & physical-type definition, range/point/downsample query
endpoints, Arrow/Parquet import-export, OpenTelemetry, Python/Rust SDKs. Also
still open from prior runs: the external-engine **DuckDB adapter** (real-database
baseline) and the other competitor adapters; the **Parquet** report format; the
rest of the hardware block (disk/GPU/driver); the standalone methodology document.

**STOP REASON:** cutoff/natural-arc — six coherent increments completed the
Phase-2 ignition arc (skeleton -> JSON interpolate -> metrics -> shared ILP crate
-> server ILP ingest -> docs), well above the 2-4 bar. A clean stopping point:
the next Phase-2 slices (query endpoints needing a stored-data model, or
Arrow/Parquet needing the arrow dependency tree) are each a fresh concern better
started on their own PR rather than half-built onto this one.

**Next step (tomorrow):** continue Phase 2 on `dsp-server`. Best next slice = a
**downsample/aggregation query endpoint** (`POST /api/v1/downsample`: min/max/avg/
count over a JSON or ILP series, reusing the same engine + metrics pattern) since
it needs no new dependency tree; OR begin **Arrow/Parquet** bench-report output
(`dsp-bench` `--parquet`), deliberately adding the `arrow`/`parquet` deps as their
own increment. Either advances the public API / interchange surface.

**PR:** https://github.com/physics515/DSP/pull/15

---

## 2026-06-16 — Phase 2 continued: dsp-server query/response surface (5 increments)

Followed the prior run's explicit "next step": continue Phase 2 on `dsp-server`
with the downsample/aggregation query, since it needs no new dependency tree.
Landed five bounded, additive increments that fill out the server's query and
response surface — all pure-Rust, no new deps, vendor-neutral — then verified
every new endpoint against a live bound server.

**Item:** roadmap Phase 2 (benchmark-grade server/API): downsample query, point
query, and the raw/interpolated/extrapolated response distinction (Phase-2
acceptance nuance + backlog item B-tags, synthetic-point marking).

**Increment 1 — `POST /api/v1/downsample`** (commit `b084e2b`)
- New `dsp-server/src/downsample.rs`: reduce a JSON series into epoch-grid-
  aligned time buckets with a per-bucket aggregate (min/max/avg/sum/first/last)
  + count. Buckets align via the engine's own `Resolution::to_base` index;
  `bucket_start` is its overflow-checked inverse. Reductions accumulate in
  `BigDecimal` (no float drift); only the final wire value narrows to f64.
  Single-pass fold over the sorted, range-windowed series. Defaults to
  [min,max,avg]. `metrics.rs` gains per-endpoint `DownsampleMetrics` (3 new
  Prometheus counters); `lib.rs` registers the route. Tests: +7 (26 lib).

**Increment 2 — `POST /api/v1/downsample/ilp`** (commit `ba65595`)
- ILP variant mirroring the interpolate JSON/ILP pair. Extracted the fold core
  into `run_downsample(points, start, end, resolution, aggregations)` over
  `splimes::Point`s, shared by both entry points (identical envelope). ILP
  payload as text/plain body; field/precision/resolution as query params and
  `agg=min,max,avg` comma-separated (`parse_aggregations`). Reuses the shared
  `dsp-line-protocol` parser; `parse_precision_token`/`parse_resolution_token`
  made `pub(crate)`. Tests: +5 (31 lib).

**Increment 3 — README + ROADMAP docs** (commit `1653df2`)
- Documented both downsample endpoints (curl examples, params, bucketing
  semantics, new counters) and recorded the downsample query as landed on the
  ROADMAP Phase-2 status note. Docs only.

**Increment 4 — `POST /api/v1/interpolate/point`** (commit `f66503c`)
- Single-instant lookup: evaluates the fitted spline at one instant by
  interpolating a minimal 1ns-wide grid at the instant and reading back the
  value there (the spline value at the instant is independent of the rest of
  the grid; `auto_interpolate` rejects a zero-span range, hence the +1ns end).
  A `ValueKind` enum labelled the result interpolated vs extrapolated. README +
  ROADMAP updated. Tests: +4 (35 lib).

**Increment 5 — per-point provenance marking** (commit `948012f`)
- Delivered the Phase-2 synthetic-point marking nuance (B-tags): unified the
  kind enum into `PointKind { Raw, Interpolated, Extrapolated }` (replacing the
  point-only `ValueKind`) + a shared `classify` helper. `OutputPoint` gains a
  `kind` field; `run_interpolation` captures input timestamps + observed span
  before the engine consumes the points and marks each grid point (raw when it
  coincides with an observation, extrapolated outside the span, else
  interpolated). The point endpoint reuses the classifier, so a lookup landing
  on an observation now reports `raw`. Additive, back-compatible. Tests: +1 net
  (36 lib).

**Build/test/clippy (real, nightly `rustc 1.98.0-nightly (cb46fbb8c 2026-06-08)`):**
- `cargo build --workspace` — GREEN (confirmed at baseline and at end).
- `cargo test -p dsp-server` — **36 lib pass** (was 19 at the start of the
  night; +17 across the five increments).
- `cargo clippy -p dsp-server --all-targets` — **0 warnings** under
  `#![warn(clippy::pedantic, clippy::nursery, clippy::all)]`; every lint hit
  (unnecessary_sort_by, too_long_first_doc_paragraph ×2, needless_pass_by_value)
  was fixed structurally, never with `#[allow]`. The 4 pre-existing `splimes`
  `sort_by_key` warnings in untouched files remain.
- `cargo fmt -p dsp-server` — clean (hard tabs, project rustfmt).
- **Live end-to-end verified** against a bound server (`127.0.0.1:18099`):
  point knot → `raw` (value 60), point past-range → `extrapolated` (value 120),
  point midpoint → `interpolated` (value 30); range provenance → first/last
  `raw`, interior `interpolated`, 61 points; downsample JSON + ILP → correct
  2-bucket aggregates; `/metrics` reflects the `dsp_downsample_*` counters.
- Scope note: tests scoped to the touched `dsp-server` crate; the change is
  confined to that one leaf crate (+ docs), and the full workspace **build** is
  green. Did not run the full (heavy wgpu/GPU/turso) `cargo test --workspace`
  to stay in the night's budget — no other crate was touched.

**Done vs open:** DONE — the `dsp-server` query/response surface now covers the
downsample/aggregation query (JSON + ILP), the single-instant point query, and
the raw/interpolated/extrapolated provenance distinction across both the range
and point responses. OPEN (Phase 2 remainder) — raw range/stored queries and
DB/subject/aspect management (need a stored-data model), schema & physical-type
definition, Arrow/Parquet import-export, OpenTelemetry, Python/Rust SDKs. Also
still open from prior runs: the external-engine **DuckDB adapter**, the other
competitor adapters, the **Parquet** bench-report format, the rest of the
hardware block, and the standalone methodology document.

**STOP REASON:** natural-arc — five coherent increments completed the
query/response arc on `dsp-server` (downsample JSON → downsample ILP → docs →
point query → provenance marking), above the 2–4 bar. A clean stopping point:
the next Phase-2 slices each pull a fresh concern onto this PR — stored range
queries need a measurement-data model, and Arrow/Parquet needs the `arrow`/
`parquet` dependency tree — so each is better started on its own PR.

**Next step (tomorrow):** continue Phase 2. Best next slice = begin the
**Arrow/Parquet interchange** as its own increment (`dsp-bench --parquet`
bench-report output, or a `dsp-server` Arrow response encoding), deliberately
adding the `arrow`/`parquet` deps as their own increment; OR start the
**stored-data / DB-subject-aspect model** that the raw range/point *storage*
queries (as opposed to the stateless compute endpoints landed so far) require —
the first slice toward Storage v2's control-plane catalog.

**PR:** https://github.com/physics515/DSP/pull/16

---

## 2026-06-18 — Phase 4.1: dsp-physical-type crate, 0→1 (5 increments)

Took a fresh roadmap track. The prior two nights completed the Phase-2
`dsp-server` query/response arc and explicitly flagged the next slices
(Arrow/Parquet deps; stored-data model) as separate concerns for their own
PRs. Rather than half-build one of those, started the next-highest not-started
foundational item: **physical numeric encodings** (Updated priority order #5;
Phase 4.1; Immediate Next Action #8) — entirely absent before tonight. Built it
from scratch as a new vendor-neutral crate through five bounded, additive,
all-green increments.

**Item:** roadmap Phase 4.1 (physical numeric encodings) / Immediate Next
Action #8. New crate `dsp-physical-type` (only `bigdecimal` + `serde`; wired
into the workspace as a member and a workspace dependency).

**Increment 1 — new crate + first three encodings** (commit `bae75c3`)
- `dsp-physical-type/{Cargo.toml,src/lib.rs}`. `PhysicalType` {`F64`,
  `ScaledI64{scale}`, `BigDecimalText`}; `PhysicalValue`; `PhysicalType::encode`
  -> `Encoded` (value + `Exactness`); `PhysicalValue::to_logical` inverse.
  The hard-constraint-#4 rule is enforced at the type level: encode always
  reports `Exact` or `Lossy{abs_error}` — a `BigDecimal -> f64` that drops
  digits is a *reported* loss, never silent. `EncodeError::{Overflow,
  NotFinite}` for unrepresentable values. Workspace root `Cargo.toml` +
  `Cargo.lock` updated. Tests: 9.

**Increment 2 — remaining three encodings** (commit `260f52b`)
- Added `F32`, `ScaledI128{scale}`, `Decimal128` to `PhysicalType`/
  `PhysicalValue`/`encode`/`to_logical`/`name`. Decimal128 = 128-bit mantissa
  with a per-value scale, fit via a digit-shedding `fit_i128` loop
  (round-half-away-from-zero `round_div_10`); it never errors, only sheds
  low-order digits (reported Lossy). Completes the six encodings Phase 4.1
  names. Tests: +7 (16).

**Increment 3 — declarative PhysicalProfile** (commit `7a1806a`)
- `PhysicalType::profile()` -> `PhysicalProfile { name, fixed_width_bytes,
  always_lossless, hot_path_eligible }` — the "each declaring storage encoding,
  eligibility, exactness guarantees" sentence of 4.1 as consumable data (F32=4,
  F64/ScaledI64=8, ScaledI128=16, Decimal128=24, text=variable; text is the only
  universally lossless / non-hot-path encoding). Refreshed the stale module-doc.
  Tests: +1 (17).

**Increment 4 — columnar batch encoding** (commit `c72d6df`)
- New `src/column.rs`: `encode_column(physical_type, &[BigDecimal]) ->
  Result<ColumnEncoding, ColumnEncodeError>` — one-pass, all-or-nothing on
  representability (first Overflow/NotFinite fails with its index; a segment
  can't mix encodings), lossy-but-representable values tallied into
  `lossy_count` + `max_abs_error`. `ColumnEncoding` exposes `is_exact`,
  `len`/`is_empty`, `decode`, and `estimated_bytes` (len*width, or summed string
  bytes for text) — the value-column term of bytes/point for Storage v2 and
  DSP-Bench. Enabled the bigdecimal `serde` feature (ColumnEncoding is
  Serialize/Deserialize). Tests: +5 (22).

**Increment 5 — advisory recommend_encoding** (commit `65db9f1`)
- `recommend_encoding(values, max_abs_error) -> ColumnEncoding`: the narrowest
  hot-path encoding holding a whole column within a tolerance (F32 4B -> F64 8B
  -> ScaledI64 8B at the column's minimal exact scale -> ScaledI128 16B ->
  Decimal128 24B -> BigDecimalText backstop). The roadmap's "fastest *safe*
  physical encoding" intent as an advisory — schema still declares; this informs
  tooling/ingest/bench. Tests: +5 (27).

**Docs** (commit `28efcaf`) — ROADMAP.md: Phase 4.1 marked started with what
shipped + what remains; Immediate Next Action #8 marked ✅ (delivered and
exceeded).

**Build/test/clippy (real, nightly `rustc 1.98.0-nightly (cb46fbb8c 2026-06-08)`):**
- `cargo build --workspace` — GREEN at baseline and at end (the new member
  builds into the workspace; nothing else regressed).
- `cargo test -p dsp-physical-type` — **27 lib pass**, 0 failed (grew 9 -> 16 ->
  17 -> 22 -> 27 across the five increments; round-trip + exactness +
  overflow/not-finite + column aggregation + recommendation behaviour).
- `cargo clippy -p dsp-physical-type --all-targets` — **0 warnings** under
  `#![warn(clippy::pedantic, clippy::nursery, clippy::all)]`. Every lint hit
  (too_long_first_doc_paragraph, const-fn len/is_empty, cmp_owned) was fixed
  structurally — no `#[allow]`.
- `cargo fmt -p dsp-physical-type` — clean (hard tabs, project rustfmt).
- Scope note: the change is one new isolated leaf crate (+ root Cargo wiring +
  ROADMAP docs); no existing crate's source was touched, and the full workspace
  **build** is green. Did not run the full (heavy wgpu/GPU/turso) `cargo test
  --workspace` to stay in the night's budget — no other crate was modified.

**Done vs open:** DONE — Immediate Next Action #8; all six Phase-4.1
`PhysicalType` encodings with explicit never-silent exactness, profile
metadata, columnar batch encode + bytes estimate, and advisory selection.
OPEN (Phase 4 remainder) — schema-level per-aspect encoding *declaration*;
wiring `estimated_bytes`/`recommend_encoding` into the DSP-Bench bytes/point
report (a schema v6 bump); timestamp semantics (4.2); the columnar segment
store itself (4.3); data skipping (4.4); Arrow arrays (4.5). Also still open
from prior runs: Phase-2 stored range/DB-subject-aspect model, Arrow/Parquet
interchange, OpenTelemetry; the external-engine DuckDB adapter and other
competitor adapters; the standalone methodology document.

**STOP REASON:** natural-arc — five coherent increments built the entire
Phase-4.1 physical-encoding foundation from nothing (crate -> all six encodings
-> profile metadata -> columnar batch -> advisory selection), above the 2–4
bar. The next step (surfacing bytes/point in the bench report) is a cross-crate
schema-versioned change (dsp-bench schema v5 -> v6 with back-compat tests) — a
distinct concern better started on its own PR than half-built onto this one, the
same discipline the prior two runs applied.

**Next step (tomorrow):** continue Phase 4. Best next slice = wire
`dsp-physical-type` into the **DSP-Bench bytes/point report** — call
`recommend_encoding`/`estimated_bytes` over a generated dataset's values and add
a `storage` block to `BenchResult` (schema v6, with a back-compat test mirroring
the existing v1–v4 fixtures), turning the new crate into a measurable
north-star-metric outcome. Alternatively begin **4.2 timestamp semantics**
(integer-epoch encoding + delta/delta-of-delta) as a sibling encoding module, or
take the Phase-2 stored-data model the server query endpoints still need.

**PR:** https://github.com/physics515/DSP/pull/17

---

## 2026-06-19 — Phase 4.1/4.2: bytes/point bench wiring + timestamp codecs (6 increments)

Continued Phase 4 from where 2026-06-18 stopped. The prior run built the
`dsp-physical-type` Phase-4.1 value encodings and logged the next step as
*"wire `dsp-physical-type` into the DSP-Bench bytes/point report"*. Took exactly
that, then drove it to a complete north-star-metric arc: the bench now reports a
**total bytes/point** over both stored columns (value + timestamp), backed by
real codecs — including the Phase-4.2 timestamp delta/delta-of-delta/RLE layer
built this run.

**Items:** roadmap Phase 4.1 (bench bytes/point wiring — the logged next step)
and Phase 4.2 (timestamp semantics, 0→started). Crates touched: `dsp-bench`
(schema v6 storage block + report rendering) and `dsp-physical-type` (new
`timestamp` module).

**Increment 1 — value bytes/point in BenchResult, schema v6** (commit `ccf26f5`)
- `dsp-bench/src/schema.rs`: new `StorageEstimate` block on `BenchResult`
  (`#[serde(default, skip_serializing_if)]`, schema v5→v6). `from_values` runs
  `recommend_encoding` over the stored value column → chosen encoding, value
  bytes, bytes/point, exactness (`lossy_count`/`max_abs_error`) within a
  tolerance. `run_profile` estimates the input dataset (the stored data) at
  tolerance 0 (narrowest lossless). Added `dsp-physical-type` dep. Back-compat
  test (v5-without-storage parses) + omitted-when-absent test. Tests: +3.

**Increment 2 — render storage in the HTML report** (commit `0edbc8b`)
- `dsp-bench/src/report.rs`: `enc` + `B/pt` columns (later widened in inc 4);
  a non-exact pick flagged with `*`; em-dashes when absent; encoding name
  escaped. Tests: +2.

**Increment 3 — Phase 4.2 timestamp delta / delta-of-delta codecs** (commit `f876aba`)
- New `dsp-physical-type/src/timestamp.rs`: `TimeUnit` (seconds/millis/micros/
  nanos); `encode_delta`/`decode_delta` and `encode_delta_of_delta`/
  `decode_delta_of_delta` — lossless `i64`-epoch difference transforms, wrapping
  arithmetic so `i64::MIN`/`i64::MAX` round-trip without panic; `zigzag_varint_len`/
  `zigzag_varint_bytes` byte estimate; `estimated_bytes()` on each column. Tests: +9.

**Increment 4 — total bytes/point (value + timestamp)** (commit `733a99e`)
- `StorageEstimate` gains the timestamp half (`timestamp_unit`,
  `timestamp_encoding`, `timestamp_bytes`, `timestamp_bytes_per_point`,
  `total_bytes_per_point`). New `from_columns(values, timestamps, unit,
  tolerance)` encodes the timestamp column delta-of-delta; `from_values` delegates
  with no timestamps. `run_profile` feeds the dataset's µs-epoch timestamps. Report
  widened to `enc` / `val B/pt` / `tot B/pt`. Tests: +1. `cargo build --workspace`
  green.

**Increment 5 — RLE for the delta-of-delta stream** (commit `3c39029`)
- `dsp-physical-type/src/timestamp.rs`: `rle_encode`/`rle_decode` (lossless
  `(value, run_length)` runs), `uvarint_len`, `rle_varint_bytes`, and
  `DeltaOfDeltaColumn::rle_estimated_bytes()` + `best_estimated_bytes()` (min of
  varint vs RLE). A regular 1000-point column's 998 zero second-differences
  collapse to one run → ~12 bytes total. Tests: +4.

**Increment 6 — bench uses the RLE-aware best timestamp cost** (commit `e663990`)
- `from_columns` now picks the cheaper of plain-varint vs RLE and records which
  (`delta_of_delta` / `delta_of_delta_rle`); the regular-series test reflects RLE
  (11 timestamp bytes for a 5-point µs series). Refreshed stale docs.

**Docs** (commit `7df5c8d`) — ROADMAP.md: Phase 4.1 bench-wiring landed; Phase
4.2 marked started with shipped codecs + what remains (bit-packing, tz/leap
policy, monotonic enforcement).

**Build/test/clippy (real, nightly `rustc 1.98.0-nightly (cb46fbb8c 2026-06-08)`):**
- `cargo build --workspace` — GREEN (verified at inc 4 after the cross-crate
  StorageEstimate API change; nothing else regressed).
- `cargo test -p dsp-physical-type` — **40 lib pass**, 0 failed (27 → 36 → 40
  across inc 3 and 5).
- `cargo test -p dsp-bench` — **86 lib + 20 integration pass**, 0 failed (run
  `--test-threads=1` for the final clean count; see PRE-EXISTING FLAKE below).
- `cargo clippy` (both crates, `--all-targets`) — **0 new warnings** under
  pedantic+nursery. Two lints hit and fixed structurally (manual `div_ceil`;
  too-long first doc paragraph) — no `#[allow]`. The only warnings emitted are 4
  pre-existing `sort_by_key` suggestions in `splimes` (untouched).
- `cargo fmt` — clean (hard tabs, project rustfmt).

**PRE-EXISTING FLAKE (not introduced this run, flagged for follow-up):**
`cargo test -p dsp-bench` under full default parallelism (~16 threads) aborts
with a single ~20 GB allocation failure (exit `0xc0000409`). Verified on `main`
@ `b8884ee`: crashes 3/3 parallel runs; passes 100% serially and was passing
once by luck. It is load-dependent (sometimes trips even at `--test-threads=4`).
Root cause is a memory-hungry path (suspect splimes interpolation / bigdecimal)
multiplied across concurrent `run_profile` tests, not the schema work here. All
this run's verification used `--test-threads=1`/`4` to get honest counts. Filed
as a background task for a dedicated fix (cap concurrency or root-cause the
allocation).

**Done vs open:** DONE — last run's logged next step (bench bytes/point wiring),
now a complete value+timestamp **total bytes/point** with real value
(`recommend_encoding`) and timestamp (delta/DoD/RLE) codecs, surfaced in the
report; Phase 4.2 timestamp codecs 0→started. OPEN (Phase 4 remainder) —
schema-level per-aspect encoding *declaration*; 4.2 bit-packing + tz/leap-second
policy + monotonic-order enforcement; **4.3 columnar segment store** (the next
big concern — deliberately not started here); 4.4 data skipping; 4.5 Arrow
arrays. Also still open from prior runs: Phase-2 stored range / DB-subject-aspect
model, Arrow/Parquet interchange, OpenTelemetry; external-engine DuckDB +
competitor adapters; the standalone methodology document.

**STOP REASON:** natural-arc — six coherent increments completed the entire
bytes/point story (value encoding in the bench → render → timestamp codecs →
total value+timestamp → RLE → RLE-aware bench), above the 2–4 bar. The next step
(Phase 4.3 columnar segment store) is a multi-increment concern with its own
on-disk format and is better started on a fresh PR than half-built onto this one
— the same discipline prior runs applied. Cutoff not reached (~03:30); stopping
on arc completeness, not the clock.

**Next step (tomorrow):** begin **Phase 4.3 columnar segment store** — the first
slice = an in-memory `.dspseg`-shaped segment type holding the typed value column
(`dsp-physical-type` encoding) + the delta/DoD/RLE timestamp column + per-segment
min/max ts/value and row/null counts, with `encode`/`decode` round-trip and a
`bytes_per_point` that matches the bench `StorageEstimate` — turning the advisory
estimate into an actual stored segment. Alternatively, the schema-level per-aspect
**encoding declaration** (`AspectSchema { value: PhysicalType, value_tolerance,
timestamp_unit }`) that both Storage v2 and the bench would consume, or 4.2
bit-packing as a sibling codec.

**PR:** https://github.com/physics515/DSP/pull/18

---

## 2026-06-20 — Phase 4.3/4.4: in-memory columnar segment + data skipping (6 increments)

Began **Phase 4.3 (columnar segment store)** exactly as the 2026-06-19 run logged
for "next step" — the first slice = an in-memory `.dspseg`-shaped segment type —
then carried it into **Phase 4.4 (data skipping)**, which the per-segment stats
exist to enable. The night built the entire in-memory segment + set-pruning
foundation: a `Segment` binding the Phase-4.1 value column and the Phase-4.2
timestamp column, per-segment min/max/count/ordering stats, a `bytes_per_point`
proven equal to the DSP-Bench `StorageEstimate`, and conservative time/value
pruning over both a single segment and a set of segments.

**Items:** roadmap Phase 4.3 (0→started) and Phase 4.4 (0→started), with seeds of
4.2 (monotonic-order) and 4.6 (out-of-order detection). Crates touched:
`dsp-physical-type` (new `segment` module + a `timestamp` codec-name helper) and
`dsp-bench` (`StorageEstimate` now shares the segment's codec selector).

**Increment 1 — in-memory typed columnar `Segment`** (commit `61169cf`)
- New `dsp-physical-type/src/segment.rs`: `Segment` / `SegmentStats` /
  `SegmentError` + `SEGMENT_FORMAT_VERSION`, re-exported from the crate root.
  `Segment::build(timestamps, values, unit, value_tolerance)` validates equal
  column heights, encodes the value column via `recommend_encoding` and the
  timestamp column via `encode_delta_of_delta`, and computes min/max ts, min/max
  value, and row count in one pass. Exact `decode`/`decode_timestamps`/
  `decode_values` round trip; the segment carries `row_count` so it resolves the
  empty-vs-lone-anchor ambiguity the timestamp codec documents. `bytes_per_point`
  sums the same value/timestamp column estimators the bench uses. serde
  round-trip (serde_json dev-dep). Tests: +8 (40→48 lib).

**Increment 2 — segment data-skipping predicates** (commit `c9bdc4b`)
- `time_range`/`value_range`, `overlaps_time`/`contains_timestamp`,
  `may_contain_value` — conservative pruning (`false` only when safe to skip).
  Tests: +3 (48→51).

**Increment 3 — one shared codec choice across bench + segment** (commit `bca9d75`)
- `dsp-physical-type`: `DeltaOfDeltaColumn::best_encoding_name()` and
  `Segment::timestamp_encoding_name()`. `dsp-bench`: `StorageEstimate::from_columns`
  now calls `best_estimated_bytes()`/`best_encoding_name()` instead of re-deriving
  the plain-vs-RLE choice inline (behaviour identical). New cross-crate test
  `storage_estimate_agrees_with_a_real_segment` asserts the bench estimate and a
  real `Segment` agree on physical type, value bytes, timestamp bytes, codec
  label, row count, and total bytes/point. Tests: dsp-physical-type +1 (51→52),
  dsp-bench lib +1 (86→87).

**Increment 4 — multi-segment time pruning** (commit `fc9ee9d`)
- `prune_by_time(segments, start, end) -> Vec<usize>`: the indices a range query
  must scan; everything else is skipped. Tests: +2 (52→54).

**Increment 5 — segment time-ordering awareness** (commit `a8c844b`)
- `SegmentStats.time_sorted` (computed in `build`) + `Segment::is_time_sorted()`:
  true admits intra-segment binary search, false flags out-of-order ingest (Phase
  4.6). Tests: +1 (54→55).

**Increment 6 — value-column multi-segment pruning** (commit `f315495`)
- `prune_by_value(segments, lo, hi) -> Vec<usize>`, the value mirror of
  `prune_by_time`. Tests: +1 (55→56).

**Docs** (commit `008c105`) — ROADMAP.md: Phase 4.3 and 4.4 marked started with
what shipped and what remains (paged on-disk layout, null column, tag/quality +
page skipping).

**Reverted mid-run (honesty):** an attempt at increment-4-as-`.dspseg`-binary-frame
(`to_bytes`/`from_bytes` via `bincode`) was backed out: `bincode` cannot
round-trip `BigDecimal` (it requires `serde::Deserializer::deserialize_any`,
which non-self-describing formats reject). This confirmed that a naive
serde-of-the-whole-struct frame is the *wrong* target anyway — Phase 4.3 wants a
hand-rolled paged columnar layout — so the slice was replaced with the
multi-segment time pruning above. No trace of the reverted attempt remains in the
tree (verified `git diff` clean before proceeding).

**Build/test/clippy (real, nightly toolchain):**
- `cargo build --workspace` — GREEN (verified after each cross-crate change:
  inc 1, 3, 5, 6).
- `cargo test -p dsp-physical-type` — **56 lib pass**, 0 failed (40→56 across the
  six increments).
- `cargo test -p dsp-bench` — **87 lib + 20 integration pass**, 0 failed (run
  `--test-threads=1` for honest counts; the pre-existing parallel-allocation
  flake documented on 2026-06-19 is unchanged and was avoided, not introduced).
- `cargo clippy -p dsp-physical-type -p dsp-bench --all-targets` — **0 new
  warnings** under pedantic+nursery. Three lints hit and fixed structurally
  (redundant closure / cast-precision in a test; `derive_partial_eq_without_eq`
  on `SegmentStats`; markdown lazy-continuation + too-long-first-paragraph in
  docs) — no `#[allow]`. The only remaining warnings are the 4 pre-existing
  `splimes` `sort_by_key` lints (untouched crate).
- `cargo fmt` — clean (hard tabs, project rustfmt).

**Done vs open:** DONE — the in-memory Phase-4.3 `Segment` (typed value + DoD/RLE
timestamp columns, per-segment stats, bench-aligned bytes/point, exact
round trip) and the Phase-4.4 data-skipping primitives (single-segment time/value
predicates + `prune_by_time`/`prune_by_value` over a set), plus time-ordering
awareness. OPEN (Phase 4 remainder) — the hand-rolled **paged on-disk `.dspseg`
layout** (page offsets / per-page stats / checksums; *not* a naive serde frame),
a quality/null column, tag/quality + intra-segment page skipping, the
catalog/metadata/segment-index DBs, Arrow/Parquet interchange, and schema-level
per-aspect encoding *declaration*. Still open from prior runs: Phase-2 stored
range / DB-subject-aspect model, OpenTelemetry; external-engine DuckDB +
competitor adapters; the standalone methodology document.

**STOP REASON:** natural-arc — six coherent increments built the entire
in-memory segment + data-skipping foundation (Segment → predicates → bench
unification → multi-segment time prune → ordering → value prune), above the 2–4
bar. The next concern (the paged on-disk `.dspseg` format) is a multi-increment
design slice with its own page/checksum layout, and this run empirically
confirmed the naive serde-frame shortcut is the wrong approach — so it is better
started on a fresh PR than half-built onto this one. Cutoff not reached (~03:15);
stopping on arc completeness, not the clock.

**Next step (tomorrow):** begin the **paged on-disk `.dspseg` layout** — a
hand-rolled binary frame for `Segment` with a fixed header (magic + version +
row/null counts + min/max ts/value + codec tags), the packed value and timestamp
column byte streams, and a trailing CRC checksum, with a versioned
`write_to`/`read_from` round trip and a corruption-detection test. Avoid `bincode`
(no `BigDecimal` support) — encode the columns' primitive byte streams directly,
keeping `BigDecimalText` values as length-prefixed UTF-8. Alternatively, the
schema-level per-aspect **encoding declaration** (`AspectSchema { value:
PhysicalType, value_tolerance, timestamp_unit }`) that both Storage v2 and the
bench would consume.

**PR:** https://github.com/physics515/DSP/pull/19

---

## 2026-06-21 — Phase 4.3 on-disk `.dspseg` frame + 4.1 schema declaration (6 increments)

Took the 2026-06-20 run's logged next step verbatim — the **hand-rolled paged
on-disk `.dspseg` layout** — and built it end to end across four slices
(primitives → value codec → timestamp codec → framed segment + corruption
detection), then landed the named alternative next step, the **schema-level
per-aspect encoding declaration** (`AspectSchema`), and recorded both state
changes in ROADMAP.md. All in one crate (`dsp-physical-type`); the workspace
stayed green throughout.

**Items:** roadmap Phase 4.3 (on-disk `.dspseg` frame: started → shipped, single-block)
and Phase 4.1 (schema-level encoding declaration: still-to-do → shipped). Crate
touched: `dsp-physical-type` only (new `dspseg` + `schema` modules; a
behaviour-preserving `SegmentStats::from_columns` refactor of `Segment::build`).

**Increment 1 — `.dspseg` byte primitives + CRC-32** (commit `2864051`)
- New `dsp-physical-type/src/dspseg.rs`: `ByteWriter`/`ByteReader` (checked
  little-endian fixed-width ints, LEB128 unsigned varints, zig-zag signed
  varints, length-prefixed byte/UTF-8 blocks) + IEEE `crc32` (const-built
  table, verified against the `123456789 → 0xCBF43926` vector) + a recoverable
  `DspSegError`. Varint widths match the crate's existing `uvarint_len` /
  `zigzag_varint_len` estimators by construction. Tests: +8 (56 → 64 lib).

**Increment 2 — value-column codec** (commit `f32dd97`)
- `write_value_column` / `read_value_column` for `ColumnEncoding`: a
  physical-type tag (+ shared scale for `ScaledI*`), value count, lossy
  bookkeeping (`lossy_count` + `max_abs_error` as decimal text), then packed
  per-value payloads — IEEE byte patterns (`F64`/`F32`), zig-zag varint mantissa
  (`ScaledI64`), full-width `i128` (`ScaledI128`/`Decimal128`), length-prefixed
  UTF-8 (`BigDecimalText`). New `DspSegError::InvalidDecimal`; added
  `put/read` i128/f64/f32 helpers. Tests: +5 (64 → 69).

**Increment 3 — timestamp-column codec** (commit `667fc9b`)
- `write_timestamp_column` / `read_timestamp_column` for `DeltaOfDeltaColumn`:
  time-unit tag, `i64` anchor, optional first delta, varint-counted zig-zag
  second-difference stream. Plain delta-of-delta on disk (RLE-on-disk is a
  later compression slice; lossless either way). New
  `DspSegError::InvalidTag { kind: "time_unit", .. }`. Tests: +3 (69 → 72).

**Increment 4 — framed segment + corruption detection** (commit `a43bee1`)
- `write_segment` / `read_segment` (+ `Segment::write_to` / `read_from`): a
  `DSPSEG\0` magic, format version, per-segment stats header (the data-skipping
  inputs), the two column blocks, and a trailing CRC-32 verified against the
  body **before** parse. Bad magic, unsupported version, and trailing bytes are
  distinct errors. Tests: +5 (72 → 77) — round-trips across shapes
  (regular/irregular/lossy-F32/high-precision-text/single/empty), a corruption
  test flipping body bytes *and* the CRC (each a `ChecksumMismatch`), and an
  end-to-end 500-point raw→bytes→raw check.

**Increment 5 — schema-declared aspect encoding + seal** (commit `e39b65e`)
- New `dsp-physical-type/src/schema.rs`: `AspectSchema { value, value_tolerance,
  timestamp_unit }` + `seal` builds a `Segment` under the *declared* encoding
  (vs `build`'s advisory `recommend_encoding`), erroring rather than silently
  downcasting — `SealError::Encode` (unrepresentable value, with index) /
  `SealError::ToleranceExceeded` (declared encoding loses more than permitted),
  enforcing hard constraint #4. Shares `SegmentStats::from_columns` with
  `build` (behaviour-preserving refactor). Tests: +8 (77 → 85).

**Increment 6 — docs** (commit `3e5be02`) — ROADMAP.md Phase 4.1 (schema
declaration shipped) and Phase 4.3 (on-disk `.dspseg` frame shipped, single-block;
remaining = quality/null column, intra-frame page subdivision, catalog DBs,
Arrow/Parquet).

**Build/test/clippy (real, nightly `rustc 1.98.0-nightly cb46fbb8c`):**
- `cargo build --workspace` — GREEN (verified after each cross-crate-visible
  change: inc 1, 4, 5).
- `cargo test -p dsp-physical-type` — **85 lib pass**, 0 failed (56 → 85 across
  the run).
- `cargo test -p dsp-bench` (single-threaded, honest counts) — **87 lib + 20
  integration pass**, 0 failed (unchanged; consumes the crate, not modified).
- `cargo test -p dsp-line-protocol` — **11 pass**, 0 failed (unchanged).
- `cargo clippy -p dsp-physical-type --all-targets` — **0 new warnings** under
  pedantic+nursery. Lints hit and fixed structurally (no `#[allow]`):
  `missing_const_for_fn` ×3 (`ByteWriter::len/is_empty`, `time_unit_from_tag`),
  `missing_panics_doc` ×3 (rewrote i128/f64/f32 reads with explicit indexing,
  matching the existing `read_i64_le`), `cmp_owned` ×2 (bound the zero),
  `redundant_clone` ×1. The only remaining workspace warnings are the 4
  pre-existing `splimes` `sort_by_key` lints (untouched crate).
- `cargo fmt` — clean (hard tabs, project rustfmt).

**Done vs open:** DONE — the complete in-memory→on-disk path for a sealed
segment: `Segment` (prior runs) now serialises to a versioned, checksummed
`.dspseg` byte frame and reads back exactly with corruption detection, and an
`AspectSchema` declares + enforces the per-aspect physical encoding. OPEN
(Phase 4 remainder) — a quality/null column (then `null_count` becomes real and
the frame version bumps); **intra-frame page subdivision** (per-page
offsets/stats — the frame is single-block today; segment-level stats + checksum
are present, page granularity is the next slice); the catalog/metadata/segment-
index DBs; Arrow/Parquet interchange; and the Storage v2 store wiring that
consumes `AspectSchema`. Still open from prior runs: Phase-2 stored range /
DB-subject-aspect model, OpenTelemetry; external-engine DuckDB + competitor
adapters; the standalone methodology document.

**STOP REASON:** natural-arc — six coherent increments built the entire on-disk
`.dspseg` frame (primitives → value codec → timestamp codec → framed segment +
corruption detection) plus the schema-declared encoding path and the docs, above
the 2–4 bar. Cutoff not reached (~03:15); stopping on arc completeness. The next
concern (intra-frame **page** subdivision with per-page offsets/stats, and the
quality/null column with its format-version bump) is a multi-increment design
slice with its own layout decisions, better started on a fresh PR than half-built
onto this one.

**Next step (tomorrow):** add the **quality/null column** to `Segment` + the
`.dspseg` frame — a third column (a presence bitmap or RLE null-run stream)
making `null_count` real, bumping `SEGMENT_FORMAT_VERSION` and adding a header
field, with seal/build accepting `Option<BigDecimal>` values and the frame
round-tripping nulls. Alternatively, begin **intra-frame page subdivision**:
split a sealed segment's columns into fixed-row pages, each with its own
min/max ts/value stats and a per-page offset table in the header, so a range
query skips pages within a segment (the Phase-4.4 intra-segment page-skipping
the single-block frame defers).

**PR:** https://github.com/physics515/DSP/pull/20

---

## 2026-06-21 (run 2) — Phase 4.3 quality/null column, end to end (4 increments)

Took the 2026-06-21 run's logged **primary** next step verbatim — the
**quality/null column** — and built it end to end across three code slices
(standalone `NullMask` → segment + on-disk frame → schema-declared seal) plus a
docs slice. This makes `SegmentStats::null_count` *real*: it has been hard-zero
since the dense `(timestamp, value)` segment shipped, because a segment had no
way to represent a row that carries a timestamp but no value. All in one crate
(`dsp-physical-type`); the workspace stayed green throughout, and each commit
left a correct frame (the in-memory model and on-disk framing landed together,
so no commit shipped a nullable segment that could not round-trip).

**Items:** roadmap Phase 4.3 (quality/null column: still-to-do → shipped) with a
`.dspseg` format-version bump (1 → 2) and a Phase-4.1 touch (`seal_nullable`
extends the schema-declared encoding path to nullable columns). Crate touched:
`dsp-physical-type` only (new `nulls` module; `segment`, `dspseg`, `schema`,
`lib` extended).

**Increment 1 — `NullMask` quality column, in-memory** (commit `b5694ab`)
- New `dsp-physical-type/src/nulls.rs`: `NullMask` — a per-row presence bitmap
  (LSB-first, `ceil(row_count/8)` bytes) recording which rows carry a value vs a
  null. A fully dense column stores **no** bytes (`bits = None`), so a
  non-nullable segment's bytes/point is unchanged. `from_raw` is the frame-reader
  entry point and validates an untrusted frame's parts (bitmap length must be
  `ceil(n/8)`; declared null count must match the clear bits) → `NullMaskError`.
  Standalone, no segment coupling. Tests: +12 (85 → 97 lib).

**Increment 2 — nullable segment + `.dspseg` quality column** (commit `94f0916`)
- `segment.rs`: `Segment` gains a `nulls: NullMask` field; `build_nullable`
  (`&[Option<BigDecimal>]` → present values stored densely + a mask),
  `decode_nullable` (realigns present values to timestamps with `None` in the
  gaps — exact inverse), `null_count`/`has_nulls`/`null_bytes` accessors
  (`null_bytes` joins `total_bytes`; zero for dense, so dense bytes/point still
  equals the DSP-Bench StorageEstimate by construction).
  `SegmentStats::from_columns_nullable` computes min/max over present values
  only; dense `from_columns` is the `null_count = 0` case. A fully-present
  `build_nullable` is byte-for-byte the dense `build`.
- `dspseg.rs`: `SEGMENT_FORMAT_VERSION` 1 → 2; a quality-column block (presence
  flag + length-prefixed LSB-first bitmap) after the timestamp column, read back
  through `NullMask::from_raw` and validated against the header row/null counts —
  a corrupt mask is the new `DspSegError::InvalidNullMask` (with `Error::source`
  chained), checked after the CRC gate. `schema.rs` `seal` carries an
  all-present mask. Tests: +8 (97 → 105 lib).

**Increment 3 — schema-declared nullable seal** (commit `69e29e4`)
- `schema.rs`: `AspectSchema::seal_nullable` seals a nullable batch under the
  *declared* `PhysicalType`, enforcing the declaration (hard constraint #4 — no
  silent downcast) over the present values only. `SealError::Encode` is remapped
  (`remap_present_index`) so an unrepresentable present value reports its
  **original** row index, not the compacted present-values position. A
  fully-present batch seals identically to dense `seal`. Tests: +5 (105 → 110 lib).

**Increment 4 — docs** (commit `b3a90ed`) — ROADMAP.md Phase 4.3: quality/null
column moved from "still to do" to shipped (NullMask, build_nullable/
decode_nullable, frame v2 quality block, seal_nullable); remaining 4.3 work
(intra-frame page subdivision, catalog/metadata/index DBs, Arrow/Parquet)
restated.

**Build/test/clippy (real, nightly `rustc 1.98.0-nightly cb46fbb8c`):**
- `cargo build --workspace` — GREEN (verified after each cross-crate-visible
  change).
- `cargo test -p dsp-physical-type` — **110 lib pass**, 0 failed (85 → 110
  across the run: +12 nulls, +8 segment/frame, +5 schema).
- `cargo test -p dsp-bench` — **87 lib + 20 integration pass**, 0 failed
  (unchanged; consumes the crate, `Segment`'s new public field did not break it).
- `cargo clippy -p dsp-physical-type --all-targets` — **0 warnings** under
  pedantic+nursery (no `#[allow]`). Lints hit and fixed structurally:
  `option_if_let_else` then `unnecessary_map_or` → `Option::is_none_or`;
  `useless_vec` in a test. Workspace clippy shows only the pre-existing untouched
  warnings (`splimes` 4× `sort_by_key`; `database`/`database_orchestration`
  significant-Drop/sort_by_key) — none in the touched or consumer crates.
- `cargo fmt -p dsp-physical-type --check` — clean (hard tabs, project rustfmt).

**Done vs open:** DONE — the Phase-4.3 quality/null column end to end: a
`Segment` can carry null rows in memory, seal them to a versioned/checksummed
`.dspseg` frame and read them back exactly (with corrupt-mask detection), and an
`AspectSchema` declares + enforces the encoding for nullable columns. `null_count`
is now real. OPEN (Phase 4 remainder) — **intra-frame page subdivision** (per-page
offsets/stats; the frame is single-block, segment-level stats + checksum present,
page granularity is the next slice); **quality/tag pruning** (Phase 4.4 — the
mask now exists; pruning on it is not yet wired); the catalog/metadata/segment-
index DBs; Arrow/Parquet interchange; and the Storage v2 store wiring that
consumes `AspectSchema`. Still open from prior runs: Phase-2 stored range /
DB-subject-aspect model, OpenTelemetry; external-engine DuckDB + competitor
adapters; the standalone methodology document.

**STOP REASON:** natural-arc — four coherent increments built the entire
quality/null column (the run's logged primary next step) from a standalone type
through the segment + on-disk frame to the schema seal, plus docs, at the top of
the 2–4 bar. The next concern (intra-frame **page** subdivision with per-page
offsets/stats, its own multi-increment layout-design slice) is better started on
a fresh PR than half-built onto this one.

**Next step (tomorrow):** begin **intra-frame page subdivision** (Phase 4.3/4.4):
split a sealed segment's columns into fixed-row pages, each with its own
min/max ts/value stats and a per-page offset table in the `.dspseg` header, so a
range query skips pages *within* a segment (the intra-segment page-skipping the
single-block frame defers) — bumping `SEGMENT_FORMAT_VERSION` to 3. Alternatively,
wire **quality pruning** (Phase 4.4): skip an all-null segment (or page) for a
value-predicate query using the now-real `null_count` / `NullMask`, and add a
present-row count over a time window.

**PR:** https://github.com/physics515/DSP/pull/21

---

## 2026-06-22 — Phase 4.3/4.4 intra-segment paging + quality skipping, end to end (7 increments)

Took the 2026-06-21 run 2's logged next step — **intra-frame page subdivision**
(plus its alternative, **quality pruning**) — and built the entire intra-segment
paging + quality-data-skipping story across six code increments plus a docs
increment. All in one crate (`dsp-physical-type`); the workspace stayed green
throughout, and every commit left a correct, round-trippable artifact (the
in-memory model and the on-disk frame landed in adjacent commits, so no commit
shipped a paged segment that could not seal/read).

**Items:** Phase 4.4 (data skipping: quality + intra-segment page skipping →
shipped), Phase 4.3 (intra-frame page subdivision with per-page stats/offsets →
shipped), Phase 4.1/4.3 (schema-declared paged seal). Crate touched:
`dsp-physical-type` only (new `page` module; `segment`, `dspseg`, `schema`, `lib`
extended).

**Increment 1 — quality-aware time pruning** (commit `7a280ae`)
- `segment.rs`: building on the now-real `NullMask`, added
  `Segment::present_count` / `is_all_null` / `present_count_in_range` and the
  free `prune_present_by_time` — skip segments that overlap a window but hold
  only nulls there (strictly more selective than `prune_by_time`). Tests: +3
  (110 → 113 lib).

**Increment 2 — intra-segment page subdivision, in-memory** (commit `03e8c00`)
- New `page.rs`: `Page` (one fixed-height row block — value column,
  delta-of-delta timestamp column, `NullMask`, and its own `SegmentStats`) and
  `PagedSegment` (partitions rows into pages of `rows_per_page`, segment-level
  rollup). `prune_pages_by_time` + `read_time_range` decode ONLY the overlapping
  pages (skipped pages' columns never reconstructed); `decode_nullable`
  concatenates exactly. `bytes_per_point` on the same ruler as
  `Segment::bytes_per_point` (coincide exactly at one page). New
  `SegmentError::EmptyPageSize`. `PAGED_SEGMENT_FORMAT_VERSION = 3`. In-memory
  only; serde round-trips. Tests: +12 (113 → 125 lib).

**Increment 3 — on-disk `.dspseg` v3 paged frame** (commit `a873b6b`)
- `dspseg.rs`: `write_paged_segment` / `read_paged_segment` (+ `PagedSegment::
  write_to` / `read_from`). Frame = magic + version 3 + `rows_per_page` +
  segment-level stats + a **per-page index table** (each page's full stats AND
  its column-block byte length) + concatenated pure-column page blocks + CRC
  verified before parse. The index lets a reader prune pages on min/max ts/value
  without touching a column byte, and seek straight to a page; each page block is
  bounded to its indexed length (under/overfull → `TrailingBytes`). Paged and
  single-block readers reject each other's frames as `UnsupportedVersion`.
  Refactor: extracted `write_segment_stats` / `read_segment_stats` (one stats
  wire shape, shared by both layouts). Tests: +4 (125 → 129 lib).

**Increment 4 — page-level quality pruning** (commit `d39469b`)
- `page.rs`: `Page::present_count` / `is_all_null` / `present_count_in_range`
  and `PagedSegment::prune_present_pages_by_time` / `present_count_in_range` —
  the page-level analogue of increment 1's segment pruning (skip all-null pages
  for a value query). Tests: +3 (129 → 132 lib).

**Increment 5 — docs** (commit `2292579`) — ROADMAP.md Phase 4.3 (page
subdivision still-to-do → shipped) and 4.4 (quality + page skipping shipped;
remaining: tag pruning, blocked on per-measurement tags not existing).

**Increment 6 — schema-declared paged seal** (commit `5deb3fb`)
- `schema.rs`: `AspectSchema::seal_paged` builds a `PagedSegment` under the
  *declared* `PhysicalType` (hard constraint #4 — the enforcing counterpart of
  `PagedSegment::build`'s advisory pick), tolerance gated PER PAGE, `Encode`
  index remapped to the global row. New `SealError::EmptyPageSize`. Tests: +6
  (132 → 138 lib).

**Increment 7 — schema-declared nullable paged seal** (commit `1eb8851`)
- `schema.rs`: `AspectSchema::seal_paged_nullable` — the quality-column
  counterpart of `seal_paged`. Per-page declaration over present values, double
  index remap (past page nulls, then past prior pages) to the global row.
  Tests: +4 (138 → 142 lib). (A docs touch folding `seal_paged` into the ROADMAP
  4.3 note rides with the wrap commit.)

**Build/test/clippy (real, nightly `rustc 1.98.0-nightly cb46fbb8c`):**
- `cargo build --workspace` — GREEN (verified after each cross-crate-visible
  change; `dsp-bench` consumes the crate and stayed green throughout).
- `cargo test -p dsp-physical-type` — **142 lib pass**, 0 failed (110 → 142
  across the run: +3 +12 +4 +3 +6 +4). Doc-tests: 0 (none defined).
- `cargo clippy -p dsp-physical-type --all-targets` — **0 warnings** under
  pedantic+nursery (no `#[allow]`). Two lints hit and fixed structurally during
  increment 2: `too_long_first_doc_paragraph` (split a const's doc) and
  `similar_names` (`all_ts`/`all_vs` → a single tuple binding).
- `cargo fmt -p dsp-physical-type --check` — clean (hard tabs, project rustfmt).
- Workspace clippy unchanged: only the pre-existing untouched warnings
  (`splimes`/`database`/`database_orchestration` `sort_by_key`/significant-Drop)
  — none in the touched crate.

**Done vs open:** DONE — the complete intra-segment paging + quality-skipping
arc: `PagedSegment` with fixed-height pages and per-page stats, an on-disk v3
`.dspseg` frame with a per-page index (prune + seek without reading columns),
quality pruning at both segment and page level, and the schema-declared paged
seal (dense + nullable) enforcing the declared encoding per page (hard
constraint #4). OPEN (Phase 4 remainder) — the **catalog/metadata/segment-index
DBs** (libSQL control plane wiring, a fresh arc touching `database`);
**Arrow/Parquet interchange** (new dep, fresh arc); **tag pruning** (Phase 4.4,
blocked on per-measurement tags / backlog B-tags not existing); the Storage v2
store wiring that consumes `AspectSchema`. Still open from prior runs: Phase-2
stored range / DB-subject-aspect model, OpenTelemetry; external-engine DuckDB +
competitor adapters; the standalone methodology document.

**STOP REASON:** natural-arc — seven increments (six code + docs) built the
entire intra-segment paging + quality-data-skipping story (the run's logged
primary next step AND its alternative) from a standalone in-memory model through
the on-disk frame to the schema-declared seal, well above the 2-4 bar. The next
concern — the **catalog/metadata/segment-index control-plane DBs** (libSQL,
touching the `database` crate) and **Arrow/Parquet interchange** (a new
dependency) — is a fresh multi-increment arc in a different area, better started
on a fresh PR than half-built onto this one.

**Next step (tomorrow):** begin the **catalog/metadata/segment-index DBs** — the
libSQL control-plane layer that *consumes* the sealed `.dspseg` segments: a
`segment_index` recording each sealed segment's `(min_ts, max_ts, min_value,
max_value, row_count, null_count, byte length, path)` so a range query prunes
segments via the control plane (turning the in-memory `prune_by_time` into a
catalog lookup) before opening any `.dspseg`. Keep it vendor-neutral and in the
control plane only (hard constraint #3 — libSQL is catalog/metadata, the
`.dspseg` segments own the measurement hot path). Alternatively, begin
**Arrow-compatible array export** (Phase 4.5) from a `Segment`/`PagedSegment`'s
typed columns (eases Python/Flight/DataFusion/Parquet) — assess the `arrow` dep's
vendor-neutrality first.

**PR:** _(opened at wrap — see below)_
