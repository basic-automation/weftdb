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
