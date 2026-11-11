# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the project is pre-1.0, minor version bumps may contain breaking changes.

## [Unreleased]
### Changed
- **README project-status table: version bump and row accuracy (EVI-22 partial).**
  Lead line now names v0.3.2 (the current release). The "Tags, client SDKs, Helm chart"
  row is split into three: "Client SDKs" names the Python, R and Rust clients and
  their publication status; "Tags (per-measurement series)" reports TAG-5..TAG-11 as
  shipped; "Helm chart" stays "Not started". The "Binaries, crates, container image"
  row now reports the image as published to ghcr.io.

### Added
- **Compatibility leg coverage gate (FRE-16).** `scripts/compat/test_leg_coverage.py`
  fails CI when a pin in `compat/releases.toml` declares a leg that no executing test
  handles. Rollback and binary compatibility are now explicitly unsupported for 1.0:
  the `upgrade` leg is the only one that exists. The v0.1.0 pin drops `rollback` from
  its `legs` array; the `LEGS` constant in `releases.py` shrinks to `{"upgrade"}`.
  Documents updated accordingly.

### Added
- **Layout-upgrade integration test (FRE-13).** `weftdb/tests/layout_upgrade.rs` covers:
  the pre-v2 (layout-1) store upgrade to layout 2 through the public API, including
  idempotency, data survival, new writes after upgrade, a synthetic marker test, and
  a reverse-direction contract note.

- **Versioned configuration schema (PAC-5a).** `docs/config-schema.md` now carries a
  `schema_version: 1` marker and a version-change table; `config/schema.toml` stores the
  machine-readable copy. `scripts/config/check_schema.py` validates that every registered
  variable is documented, the version markers agree, and the checker runs in the `claims`
  CI job. (#pending)

- **Security documentation updated (SEC-14).** `docs/security.md` rewritten with source
  citations to `weft-server/src/auth.rs`, `tls.rs`, `host_guard.rs`, `audit.rs`,
  `settings.rs`, `routes.rs`, `main.rs`, and `cli.rs`. Adds the full six-role table,
  credential-in-query guard, proxy guard, exit statuses, startup enforcement section,
  and a section on security-relevant absences. See the branch diff for the full delta
  from the previous version of the document.

- **Operator documentation** ([docs/operations.md](docs/operations.md)) — backup and
  restore, systemd, container, upgrade, and failure modes with what to do.
- **SOA-15 honest bookkeeping for TTL lane.** `docs/soak.md` and `ROADMAP.md` now
  document that the TTL lane is blocked because the store has no row-level TTL
  mechanism (`weftdb/src/types/segment_store.rs:1982-1992` only purges expired
  quarantine records; the soak driver uses `NullServer` so no endpoint can be
  exercised). The nightly workflow (parts 1 and 3 of SOA-15) was already present
  on `origin/main` with active cron, first-night-no-baseline, and SHA-pinned
  actions.
- **Configuration schema v2.** `docs/config-schema.md` now documents `WEFT_DISK_RESERVE_BYTES`
  (one new Tuning variable). `schema_version` is bumped to 2 in both the doc and
  `config/schema.toml`.

### Fixed
- **The `/ready` source comments still described a Windows no-op disk guard (#674).** `DiskGuardState::disk_refusing`'s doc comment and the `/ready` handler comment claimed the guard could not measure free space on Windows, which stopped being true when `disk_space_at` grew its `GetDiskFreeSpaceExW` leg. Both now name the probe per platform and reserve "always false" for platforms with no probe. Comments only; no behaviour change.
- **The `/ready` platform note contradicted the shipped disk guard (#674).** `docs/reference.md` still claimed the guard uses Unix `statvfs` and that `disk_refusing` is always `false` on Windows, which stopped being true when the `GetDiskFreeSpaceExW` leg landed. The note now says free space is measured on both platforms and the guard is inert only where the OS cannot describe free space.
- **`free_bytes_at` now works on Windows and other platforms (#667).** The disk guard was inert on Windows, making `GET /ready`'s `disk_refusing` permanently false. Implemented via inline FFI to `GetDiskFreeSpaceExW` (no new dependencies).

- **A disk-full IO error anywhere in the chain now produces `507 insufficient_storage` instead of a generic 500 (#656).** `error.rs` adds `error_chain_is_disk_full`, checked in `classify_ingest` and `classify_read` before the fallback to `Internal`.

- **`host-install.yml` carried the one unpinned action on `main` (#626).**
  `.github/workflows/host-install.yml:67` still asked for `Swatinem/rust-cache@v2`
  while every other workflow was pinned to `e9d7b9f1` by #628, because the workflow
  arrived in #596 after that branch was cut. It merged into a tree that already had
  `scripts/ci/check-action-pinning.py`, so `main` went red on the pinning gate the
  moment it landed — the gate works, the merge queue just did not re-run it.
- **Test suites and refusal lists now accept `vX.Y.Z-beta.N` (TAG-11, #657).** The tag grammar was extended to accept `-beta.N` tags but the two test suites that pin the old grammar were not updated. `scripts/compat/test_releases.py` now parses the new 5-field `semver_key` tuple and asserts the beta-sorts-before-rc-sorts-before-release ordering; `scripts/release/test-stage.sh` accepts `v1.2.3-beta.1` and refuses the malformed `v1.2.3-beta` (no number).

- **The crash matrix's SIGKILL cells now run on Linux only (turso#7480).** A SIGKILL
  that lands between Turso's logical-log truncation and its WAL truncation leaves a
  `.db-wal` holding committed frames with no `.db-log` header, and Turso 0.8.1 refuses
  to reopen that database:
  `WAL has committed frames but logical log header is missing`.
  [tursodatabase/turso#7480](https://github.com/tursodatabase/turso/issues/7480) has
  been open since 2026-06-12. WeftDB stays fail-closed: no orphan WAL is deleted or
  quarantined at open (the engine keeps the backfill evidence in internal state, so
  discarding the file could lose acknowledged writes), and the experimental
  passive-checkpoint flag stays off. `kill-ingest` and `kill-compaction` now run where
  the dedicated `crash-matrix.yml` job already ran them — Linux — and the integration
  test names the cells it skipped and why on every other platform. macOS and Windows
  runners lose SIGKILL coverage; the Linux job keeps it. See
  [docs/upstream/turso-7480-reopen-refusal.md](docs/upstream/turso-7480-reopen-refusal.md).
- **The value-band read is documented as a raw scan, with a test pinning it against the point and time-range fold (#665).** The value-band read (`read_value_range`/`read_value_range_of`) is a raw scan: it returns every stored row whose value is in `[lo, hi]` in segment-seal then in-segment order and does **not** fold overlapping segments. Point and time-range reads use newest-wins folding; the value band answers "all raw observations." `docs/upgrade-notes.md` and `docs/reference.md` now state this split explicitly, and a synthetic-collision test (`value_range_is_a_raw_scan_pinned_against_point_and_range`) pins the value-band raw scan beside the point/range fold.

### Changed

- **One float-input rule across every ingest format (FRE-8b, #313).** ILP float fields
  and Parquet/Arrow `Float64`/`Float32` cells now enter as their shortest round-trip
  decimal (decision 11) through the new `weft_physical_type::decimal_from_f64` /
  `decimal_from_f32`, instead of the exact binary expansion `BigDecimal::from_f64`
  produced: an ILP or Parquet `0.1` now stores `0.1`, the same value a JSON or CSV
  `0.1` stores. The smallest subnormal, previously refused because its expansion's
  plain form exceeds the decimal domain, now enters as `5e-324`.

- **Breaking (read shape): a range read now returns each timestamp once, newest value
  wins (#443).** `read_time_range_of` now opens all segments overlapping the window (including
  all-null ones) and folds them with `merge_newer_wins`, so a newer segment's null
  overrides an older value at the same instant.  Point reads keep present-prune and
  skip `None` from the frame reader, so a newer all-null segment does **not** mask an
  older value there — the resulting asymmetry between range and point is deliberate
  and pinned by a test.

- **Fill tokens are exact-case** (PAC-3, #453). `Fill::from_token` now matches the
  lowercase wire tokens exactly, so `?fill=`, the downsample JSON body's `fill`,
  the stored-range `fill=` and `weft-bench --gf-fill` refuse `Linear`, `CUBIC` and
  `NULL` with a `400` (or a CLI error) that names the token. **This breaks v0.3.x
  clients** that send mixed-case `?fill=Linear` or `--gf-fill=CUBIC`: send `null`,
  `prev`, `linear`, `quadratic`, `cubic` or the lowercase spellings. The value arm
  (a decimal constant) is unchanged.
- **`/v1` range responses may omit `total` or send it `null` (cursor pages).**
  The `points`, `range`, `value-points` and `value-range` JSON endpoints no longer
  promise `total` on every page: it is `null` on cursor-following pages or absent
  from the response body. The Rust client now models it as `Option<usize>` with
  `#[serde(default)]`; a strict client of any language must treat it as optional.

### Removed

- **`TEST_DATA_DIR` is gone** (PAC-6, completing the PAC-5a configuration
  registry). The test-only variable no longer overrides the database root in
  `weftdb::data_dir`; the test suites are handed their fixture directory
  through the Stable `WEFT_DATA_DIR` variable instead. Operators were never
  meant to set it — it was a Test-tier registry entry — but a deployment that
  did will now find it ignored, with `WEFT_DATA_DIR` as the migration.

### Added

- **Pagination cursor v1 for stored-range reads (ROB-15).** The JSON
  `{/value-}points` endpoints now emit a selector-hashed cursor (`next_cursor`)
  on every page until the end of the window. A client follows it by re-issuing
  the request with `?cursor=…`; the cursor supersedes `offset`/`page`. A cursor
  whose selector, high-water mark or data fingerprint no longer matches the
  server's current view returns `410 cursor_stale`. `total` is present on the first page and `null` on
  cursor-following pages.
- **Stability and versioning policy (FRE-15).** [`docs/stability.md`](docs/stability.md)
  documents the semver policy, the surface stability table (query/ingest wire API,
  `weftdb-client` Rust SDK, storage format, CLI, configuration, metrics), what the compat
  suite actually tests, and the promises the repo does not yet support. Linked from
  `README.md` and `docs/reference.md`. (#331)
- **UPGRADING.md and the upgrade/rollback compat legs (PAC-14a).**
  [`UPGRADING.md`](UPGRADING.md) documents the layout 1→2 upgrade, rollback
  procedures and version support window. The v0.1.0 release pin in
  `compat/releases.toml` carries `legs = ["upgrade", "rollback"]`; the upgrade
  leg is tested by `weftdb/tests/compat_v010.rs`, and the rollback-after-writes
  leg is documented as aspirational.

- **The measurement rules are wired to the figures they govern (EVI-6, #430).**
  `tools/check_claims.py` now fails when the claims document never links
  `docs/methodology.md`, or when that file is missing under the repository root, so a
  published number cannot outlive the rules that made it. `README.md` and
  `docs/reference.md` carry the link; the note in `docs/claims-audit.md` that the
  methodology "does not yet exist" was stale — it landed in #630 — and now says where the
  rules live.
- **Guard the `#[serial]`-above-`#[test]` ordering (#405).**
  `scripts/ci/check-serial-order.py` scans every tracked `*.rs` file and
  fails, with `file:line`, on any `#[serial]` that is not immediately above
  a test attribute (`#[test]` or `#[tokio::test]`). Wired into the `claims`
  job in `ci.yml` after the action-pinning check.

- **The frame-fault policy is verified (ROB-22/C5).** The three behaviours of
  the C5 policy are now pinned by tests: a suspect frame is excluded from
  reads, a suspect frame is never auto-purged by maintenance, and a corrupt
  frame is quarantined and then TTL-purged. The first was previously asserted
  only through the recovery report, and the third not at all. (#446)

- **Six-field claims audit of `docs/reference.md` (EVI-6/#430).**
  `docs/claims-audit.md` audits every published performance number against the
  six-field provenance rule (source, command, dataset, measured-date,
  reproducible-from-clone, artifact). 11 registered and 19 unregistered figures
  are assessed for conformance; three non-recoverable claims carry a
  pre-six-field note in the reference doc; the single fully-conforming claim
  (pointread bench) carries its fields inline. Non-conforming figures must be
  re-measured before they are repeated — they are not deleted.
 ([EVI-6] audit the reference documentation against the six-field rule (#430))

- **Generated OpenAPI document from the route registry (FRE-9a).** The
  document at `compat/openapi/weftdb.json` is built from the same
  `RouteTable` the HTTP router serves, so every path, method, scope,
  admission class and tier is the one the code registered. The drift
  gate (`generated_spec_matches_committed_file`) checks the committed
  file matches the generated output; regenerate with `weft-server openapi
  --out compat/openapi/weftdb.json`. The subcommand prints to stdout with
  no `--out`, or writes to a named file.
- **Nightly soak comparison gate (SOA-15).** A scheduled + dispatchable soak
  workflow (`soak.yml`) builds the soak driver, runs a 15-minute soak,
  evaluates the SoakReport against SLO rules R1–R8, and compares the result
  to the previous night's artifact. A Pass→Fail rule regression turns the job
  red; metric drift past documented tolerance produces `::warning::`
  annotations. The comparison function lives in
  `weft-bench/src/soak_compare.rs`, and the annotator script in
  `scripts/annotate-soak-drift.py`. Documented in `docs/soak.md`.

- **Soak upgrade lane (SOA-16).** `weft-bench/src/soak_upgrade.rs` judges
  the store an in-soak upgrade leaves behind against the run's whole
  acknowledged history, exactly, and records the upgrade step (the
  versions on either side, the store layouts, and the pass's duration) on
  the run record (`SoakHarness::record_upgrade_verdict`, soak schema v7).
  The caller performs the upgrade per `UPGRADING.md` (PAC-14a) and hands
  the read-back to the checker, on the SOA-11 backup lane precedent; the
  lane adds no SLO rule. The nightly comparison tracks
  `upgrade_ticks_judged` (0 %), `upgrade_mismatch_count` (0 %) and
  `upgrade_duration_ms` (10 %). Documented in `docs/soak.md`.

- **Performance methodology document.** `docs/methodology.md` defines what
  counts as a performance claim, what evidence a claim must carry, what
  disqualifies a claim, and where numbers may be stated. It makes a published
  number checkable; it does not yet certify that the numbers published before it
  satisfy it, which is the audit the section on Rust doc comments points to.
  (#430)

- **Release tag grammar extended to `vX.Y.Z-beta.N` (TAG-11, #336).** The
  `scripts/release/verify.sh` `TAG_RE`, `scripts/release/stage.sh` `TAG_RE`,
  `scripts/compat/releases.py`, the inline regex in `.github/workflows/publish.yml`
  and the tag grammar documentation in `RELEASING.md` now all accept
  `vX.Y.Z-beta.N` alongside the existing `vX.Y.Z` and `vX.Y.Z-rc.N` shapes.
  Pre-release ordering follows semver 2.0: `-beta.N < -rc.N < release`. (#336)

### Changed

- **CI now pins every GitHub Action to a commit SHA.** A floating `uses:` (e.g.
  `Swatinem/rust-cache@v2`) fails the static-check job. A new guard,
  `scripts/ci/check-action-pinning.py`, scans every workflow on every push and
  pull request. (#626)
- **The OpenAPI drift gate no longer fails on a version bump (#620).**
  `generated_spec_matches_committed_file` now normalises `info.version` on
  both sides before comparing, so every path, method, scope, tier and schema
  is still checked but a release version change does not trip the gate.
  A separate assertion in `info_block_has_service_name_and_version` verifies
  that `info.version` equals the crate version directly.

- **A benchmark report with more than one result verifies.** `weft-bench`'s run manifest graded
  each measure by metric name alone, so two results in one report — a crash matrix, a query mix, any
  cross-adapter run — looked like a duplicated measure and `finish --manifest` refused the
  evidence (#517). Measures now carry the result they were quoted from (`profile`, `adapter`,
  `workload`) and uniqueness is checked within a result, so the same metric may appear once per
  result; a repeat inside one result is still a defect, and the message names both the metric and the
  result. Manifest schema v23 → v24: a manifest written by an older build is refused with an
  instruction to re-run the benchmark, since the fields that identify a measure did not exist then.

- **`weft-xbench combine` no longer calls an unreproducible-but-in-tolerance answer a failure (#516).**
  Two runs that each pass their own tolerance but produce different answer digests are
  non-deterministic, not wrong. The combined gate is reported as `non-deterministic` and kept out of
  win/loss verdicts and failure lists; a new `Non-deterministic within tolerance` section in the
  rendered report explains the row. Report schema v7 → v8: `Correctness` gains an `indeterminate`
  field.

### Fixed

- `weft-orchestration`'s seven slow tests are bounded by a 1200-second `tokio::time::timeout` each, so a hang names the test and fails the run instead of disappearing at the job's 60-minute cap (#404, #646). The bound is 3x the window in which all seven finished for real on a runner (357 s in the run where the seeded `Crypto` database was present); a push or pull-request run skips them, so the nightly schedule is what exercises them.

- **A competitor result with no adapter report refuses to take part in a
  comparison (EVI-8B, #452).** `weft-bench compare` compared a `duckdb` arm whose
  `adapter_info` was `null` with a WeftDB arm and footnoted the missing attribution
  instead of stopping, so a ratio claim could be published about numbers nobody could
  attribute to an engine. `provenance` now emits a `hard:` note per such result, which
  reuses the existing `Incomparable` refusal and the existing `--ignore-provenance`
  override; the predicate is `adapter_info.is_none() &&
  !adapter.starts_with("weftdb")`, because `weftdb`, `weftdb-stored` and
  `weftdb-ingest` measure their own paths and honestly report no adapter, and because
  `native_engine` lives inside the `AdapterInfo` whose absence is the thing being
  tested.

- **Prediction signals are now isolated per aspect (issue #402).**
  `SIGNALS_QUEUE` was a single process-wide `Mutex<Signals>`; two pipelines
  over different aspects in one process would see each other's signals.
  The queue is now keyed by `AspectId` (`Mutex<HashMap<AspectId, Signals>>`),
  `signals_for(aspect_id)` copies out only that aspect's queue,
  `create_signals` and `filter_expired_signals` touch only their own aspect,
  and `Pipeline::generate_signals` / `Pipeline::filter_signals_at` read
  only their own aspect's signals via `signals_for`. An empty per-aspect
  queue is removed from the map during `filter_expired_signals` so the map
  does not retain stale entries indefinitely. The same pass no longer holds the
  queue lock while it talks to the database: the event and correlation reads
  happen before the lock, one synchronous function mutates the queue, and the
  correlation writes happen after it is released, so a busy aspect can no longer
  stall another aspect's pass on I/O. (#402)

- **Slow orchestration tests bounded by timeout (issue #404).** Seven
  `weft-orchestration` tests that self-skip locally (Crypto-database guard)
  now wrap their bodies in `tokio::time::timeout` within the CI job budget
  (3600 s job timeout): `test_api` (60 s), `test_api_precise` (900 s),
  `test_pipeline_api` (1500 s), `test_pipeline_api_precise` (600 s),
  `test_batch_processing` (10 s), `test_specific_process_batch` (10 s) and
  `test_pipeline_api_with_fake_db` (10 s). The predecessor's 3600 s bound
  for `test_api_precise` could never fire before the job was killed; the
  two 1800 s bounds triggered `clippy::duration_suboptimal_units`. All
  corrected. Sum of worst-case sequential bounds: 3090 s, leaving ≈510 s
  for compilation, teardown and crash verification. Durations measured
  from run 40886128666.
## [0.3.2] - 2026-11-06

### Fixed

- **The abort arm of the crash matrix no longer flakes on Windows.** The test aborts a
  child process mid-operation and then re-opens the same store in the parent, and Windows
  keeps an aborted process's Turso database handles live for a short window, so those first
  opens returned `I/O error (tempdir): permission denied` while Linux tolerated them.
  `verify_recovered` now retries the open on Windows (6 attempts, 500 ms apart) and
  propagates any other error unchanged; no production path opens a path an aborted process
  just vacated, so the race was test-only (#625).
- **A pipeline's pattern dictionary no longer grows without bound.** Each
  `Pipeline::run` loaded the stored dictionary on top of the patterns it already held
  and then appended the whole dictionary to the store again (`patterns.id` is not
  unique), so every run roughly doubled it, and each duplicated row then loaded all of
  its pattern's occurrence and relative rows. A Fan Control daemon reached 3.5 GB of
  `preamble.db` and 112 GB of RSS within minutes. `load_dictionary` now starts from the
  store, extraction writes back with the new atomic
  `Database::replace_dictionary_patterns`, `Dictionary::import_pattern` ignores a
  pattern id it already holds, and the dictionary-list cache key now includes the aspect
  (two aspects with a dictionary of the same name were served each other's patterns).


### Fixed

- **The host-install gate that proves gate 4 can fail was pointing at a Stable variable.** Its
  Test-tier probe name came from `grep -B3 'tier: Tier::Test' weft-server/src/settings.rs`, and
  every `VarSpec` is one line, so the three context lines above each match returned the *entry
  above* the Test entry as well. The first name it therefore picked was
  `OTEL_EXPORTER_OTLP_ENDPOINT` — `Tier::Stable`, meant to be accepted — and the gate reported
  that a valid variable "was refused for some reason other than its tier". The name is now read
  off the matched line itself, so the probe carries a real harness channel
  (`WEFT_BENCH_BACKUP_DIR` today) and the refusal the gate greps for is the one the server
  actually produces. No server behaviour changed: strict configuration had been refusing
  Test-tier channels correctly all along.

## [0.3.1] - 2026-11-06

### Fixed

- **docs.rs builds for `weftdb`, `weft-arrow-store` and `weft-orchestration`.** 0.3.0's
  documentation failed to build: `turso_core` pulls in `branches` 0.4.x with its `std`
  feature off, and that path calls `core::intrinsics::abort`, which nightly removed on
  2026-10-15. The workspace `[patch]` only covers local builds, so `weftdb` now depends on
  `branches` with `std` enabled, which carries into the published crate.

### Changed

- The Rust SDK is published as **`weftdb-client`** (library `weftdb_client`). The
  `weft-client` name on crates.io belongs to an unrelated crate, so v0.3.0 shipped
  without the SDK on the registry. The source stays in `weft-client/`.
### Changed

- **A refused bind address names the variable.** `WEFT_SERVER_ADDR` and `WEFT_ADMIN_ADDR`
  now refuse to start the server with `WEFT_SERVER_ADDR="127.0.0.1" is not a socket address
  (invalid socket address syntax); e.g. 127.0.0.1:8080` instead of the parser's bare
  `invalid socket address syntax`, in the same shape as `WEFT_FLIGHT_ADDR` and the
  strict-config warnings already use. Exit status is unchanged (78). This is what lets
  `scripts/packaging/test-host-install.sh` gate 4 prove that the systemd env file it
  installed is the configuration the running binary read.

### Added

- **Disk guard, headroom API and space gauges** (ROB-17). An ingest or seal that would
  leave the store's filesystem with less than `WEFT_DISK_RESERVE_BYTES` available (default
  256 MiB; `0` turns it off) is refused `507 insufficient_storage` before any frame is
  written (`weftdb::DiskGuard`, `StoreError::InsufficientStorage`). The experimental
  `GET /api/v1/storage/headroom` (`metrics` scope) reports the filesystem's size, available
  bytes, the reserve and what is still writable, and `/metrics` carries the same numbers as
  the `weft_disk_{total,available,reserve,writable}_bytes` gauges plus a 0/1
  `weft_disk_guard_refusing`. The library default
  (`SegmentStoreOptions::default`) leaves the guard off. The refusal is logged once when
  the guard starts refusing (WARN) and once when it admits again (INFO), not per request. `GET /ready`
  gains `disk_refusing` (a boolean; `ready` stays `true`, since reads keep working).
  Both SDKs read the headroom: `weft_client::Client::headroom()` (typed `Headroom`) and
  the Python `Client.headroom()`.
- **Mutation harness for frames, sidecars and Parquet** (ROB-9). Every committed golden
  `.weftseg` frame, a tiered `.weftpart` sidecar and a WeftDB-written Parquet file are
  mutated — every byte substitution, every truncation, and a seeded multi-byte corpus —
  and **resealed** (CRC recomputed, so the mutation reaches the decoders) before going
  through every reader; none may panic.

### Fixed

- **A resealed frame whose header claims 0 rows no longer panics the windowed range read**
  (ROB-9). The closed-form range window now cross-checks the header's row count against
  the timestamp column, as the full decode already did, and refuses a disagreement as
  `ColumnLengthMismatch`.
- **A Parquet body that panics the `parquet` decoder is `400 bad_request`, not
  `500 commit_failed`** (ROB-9). `weft_arrow::read_parquet` contains the decoder's own
  panics (three found in `parquet` 60.0.0 within single-byte edits of a valid file) as
  `ConvertError::Parquet`.
- **JSON floats are parsed to the nearest `f64` in every build of `weft-server`.** serde_json's
  default float parser is not correctly rounded, and a `weft-server` built on its own — as
  the release workflow and the Docker image build it — read some JSON numbers one ULP away
  from their text (for example `900719925474100.5`; 175,103 of a seeded million values
  shaped like the compute routes' inputs); a whole-workspace build happened to
  read them right through another crate's `arbitrary_precision`. The workspace now enables
  serde_json's `float_roundtrip`.

## [0.3.0] - 2026-11-05

### Added

- **GPU configuration, compute limits, interpolation provenance and metrics** (ROB-16).
  `WEFT_GPU_BACKEND` (`auto` / `cpu` / `parallel` / `gpu`) pins the compute backend the
  interpolation endpoints run on, beside the existing startup calibration; the configured
  `WEFT_MAX_INTERPOLATE_POINTS` cap now also bounds the downsample gap-fill grids, which
  previously used the 10,000,000 default constant regardless of the setting. Interpolate
  responses carry each point's provenance in the open `ValueKind` vocabulary (the wire
  tokens `raw` / `interpolated` / `extrapolated` are unchanged) and name the backend that
  actually produced the values (`backend`, plus `gpu_fallback` when `auto` tried the GPU
  and fell back); `/metrics` gains per-kind output-point counters, per-backend request
  counters and a GPU-fallback counter, also present in `/debug/profile/current`.

- **Probe-only admin listener** (ROB-23r). With `WEFT_ADMIN_ADDR` set to a loopback address
  (e.g. `127.0.0.1:9095`), `weft-server` binds a second listener serving only `/health`,
  `/ready`, `/healthz` and `/readyz` — with no auth, no host guard, and no other route. An
  HTTP/1.0 probe with no `Host` header passes (the bind itself is the protection), and
  supervisors (systemd, Docker `HEALTHCHECK`, k8s) need not expose or route to the data
  listener. Unset (the default) means no second listener.
- **Upsert keys** (Phase 4.6). `POST /api/v1/storage/aspects` takes an optional
  `upsert_keys` list, `"timestamp"` plus tag keys (`SegmentStore::declare_with_upsert_keys`
  in the library). Every ingested row's tags must then fall inside those keys: a tag outside
  them is `400 invalid_tag` (`details.reason` `not an upsert key`). The keys are immutable and
  part of the declaration, so other keys (or none) on a re-declare is `409 schema_conflict`,
  and the declare response and the schema endpoint echo them. Store migration
  `0008_upsert_keys` (layout 2) adds the nullable column they live in; an older layout-2
  build opens the store and ignores the declaration.
- **Skip-identical overlap merge** (Phase 4.6). When an overlap group's newer-wins fold is
  exactly its oldest member's own rows (a replayed batch), the merge writes no new frame:
  it keeps that member and deletes the rest. Values are compared with their scale, so a
  replay of `5` as `5.0` is still rewritten.
- **R client** (`clients/r`, package `weftdb`): stored windows, value bands and server-side
  downsamples as Arrow tables, exact JSON rows paged by cursor, interpolation, and ingest of a
  data frame or Arrow table as one Parquet batch with `Idempotency-Key`. Refusals are
  conditions of class `weft_<code>` carrying `status`, `details` and `request_id`.
- **Grafana JSON-datasource routes** (experimental, `read` scope): `GET /api/v1/grafana`,
  `POST /api/v1/grafana/metrics` and `POST /api/v1/grafana/query` answer the community
  `simpod-json-datasource` plugin, so a dashboard charts stored aspects with each panel's
  buckets (its interval, capped at its `maxDataPoints`) reduced by the stored downsample.
  `packaging/grafana/weftdb-datasource.yaml` provisions it.
- **Arrow Flight listener** (experimental, read-only). With `WEFT_FLIGHT_ADDR` set to a
  loopback address, `weft-server` serves Arrow Flight over gRPC beside the HTTP listener:
  `DoGet` streams a stored time window or value band (a JSON ticket) as the same typed record
  batch the Arrow range routes encode, and `GetFlightInfo`, `GetSchema` and `ListFlights`
  describe the declared aspects. Keys and the `read` scope apply as on HTTP, denials are
  audited, a query gate sized by `WEFT_QUERY_MAX_INFLIGHT` sheds load as `UNAVAILABLE`, and a
  refusal carries its envelope code in `x-weft-error-code`. Writes, Flight SQL and a
  non-loopback (plaintext) bind are refused.
- **Rust SDK** (`weft-client`). An async client for every route of the HTTP API, on
  `reqwest`: compute, catalog, the four ingest formats with `Idempotency-Key`, stored reads
  with cursor-following `Stream`s, maintenance, backup and the bench runs. Stored values are
  written from any `StoredValue` (decimal text, `BigDecimal`, integers) and read back as
  `BigDecimal`; `f64` is not a `StoredValue`, so a float cannot reach the store by accident.
  Refusals are one `Error` carrying the whole envelope (`code`, `details`, `limit`,
  `committed`, `request_id`); an overloaded read is retried after `Retry-After`, a write
  never is. Features: `rustls` (HTTPS on the ring provider, platform or supplied roots) and
  `arrow` (decode Arrow IPC and Parquet answers to `RecordBatch`es).
- **Python SDK** (`clients/python`, package `weftdb`). A standard-library client for every
  route of the HTTP API: compute (`interpolate`, `downsample`, their ILP forms), catalog,
  ingest (JSON points, ILP, CSV, Parquet or a `pyarrow.Table`, with `Idempotency-Key`),
  stored reads with cursor-following iterators, maintenance and backup. Stored values travel
  as `Decimal`/text and a `float` is refused at that boundary; Arrow and Parquet answers
  decode to `pyarrow.Table` with the optional `arrow` extra. Refusals raise one exception
  class per envelope family carrying `code`, `details` and `request_id`, and an overloaded
  read is retried after `Retry-After`.
- **`GET /bench/runs` and `GET /bench/runs/{id}`** (experimental, `metrics` scope). With
  `WEFT_BENCH_RUNS_DIR` set, `weft-server` lists the weft-bench run manifests under that
  directory and serves one byte for byte by its run id, the SHA-256 of the manifest file;
  `weft-bench` prints the id beside the manifest path. An unknown id is the new
  `404 run_not_found`.

- **Ingest ceilings (ROB-7).** Two more numbers bound what one ingest body can
  become, in all four formats: `WEFT_MAX_INGEST_ROWS` (default 10,000,000 rows,
  refused `413 too_many_rows`) and `WEFT_MAX_INGEST_DECODED_BYTES` (default
  1 GiB, refused `413 payload_too_large`). Both are measured on the columns the
  format's parser produced rather than on the bytes that arrived — a body cap
  cannot bound what a compressed Parquet file expands to, and a row count cannot
  bound what those rows weigh once the decimals are wide. The byte ceiling counts
  a 16-byte slot per timestamp and per value, plus the mantissa bytes the
  non-null decimals actually need.
- **RPO/RTO (EVI-19).** New [`docs/RPO-RTO.md`](docs/RPO-RTO.md) pins the
  recovery-point and recovery-time objectives against the snapshot cadence: the
  corrected formula `RPO = max(I, d_max) + d_max`, the backup settings that set
  `I`, and the restore paths that set RTO — with every figure measured, derived
  or labelled unmeasured.

- **The scope pin and `GET /api/v1/version` (FRE-12b).** A store root holds many `(database,
  subject)` namespaces, so a server whose `WEFT_STORE_DATABASE`/`WEFT_STORE_SUBJECT` name one
  the root does not hold does not fail: it serves an empty namespace, which is what a lost
  store looks like from the outside. `weft-server` now compares the scope it is configured for
  against the scope the root's own `STORE_FORMAT` records before it opens, and refuses a
  mismatch with exit `65` — the operator-action class, where "scope mismatch" has always sat in
  the exit table without the server ever producing one. The message names both scopes, the
  root, and the three ways out. `WEFT_STORE_SCOPE_PIN=off` serves the root under the configured
  scope anyway, for a deployment that means it; a root with no marker yet has no recorded scope
  to refuse against. The new `GET /api/v1/version` answers the same questions over HTTP — the
  build (OS, architecture, libc, debug or release, and the target triple, compiler and commit
  it was built from, recorded at compile time because a released box has neither to ask), the
  store's layout numbers beside what this build can write, the scope being served against the
  scope the store records, and the whole exit-status table — from process state alone, so it
  answers while a store is being refused. It takes a key where keys are mounted: `/health` and
  `/ready` stay the anonymous surface.
- **Read ceilings (ROB-12, decision 12).** `weft-server` refuses a read whose
  result shape is too big instead of attempting it: a page over
  `WEFT_MAX_PAGE_ROWS` (100,000 rows), a window over `WEFT_MAX_READ_ROWS`
  (5,000,000 rows), a downsample grid over `WEFT_MAX_DOWNSAMPLE_BUCKETS`
  (1,000,000 buckets) and an `at-multi` batch over `WEFT_MAX_AT_MULTI_INSTANTS`
  (10,000 instants) are each a `422 result_too_large` carrying the limit it hit,
  and a read still running when `WEFT_READ_TIMEOUT_SECS` (30) expires is a
  `504 query_timeout`. The page ceiling is enforced before the store is opened;
  the row ceiling bounds the window rather than the page, because the store
  materializes the rows a read names; the bucket ceiling is counted from the
  request's own bounds when it names both and from the grid produced otherwise.
  `/ready` and `/health` are never bounded by the read deadline.

- **Audit log (SEC-9).** `weft-server` now keeps an fsynced append-only JSONL audit log
  (decision 8): every admin or mutating operation — ingest, declare, maintenance, backup —
  gets an attempt record before it runs and an outcome record after, and the operation is
  refused `503 audit_unavailable` when either record cannot be written (fail-closed).
  Committed ingests, authentication denials and daemon panics land in the same file as
  `weft_audit` events. `WEFT_AUDIT_FILE` names the file; unset, it lives at
  `<store root>/audit.jsonl` (or `./weft-audit.jsonl` with no store configured), and a
  server that cannot open it refuses to start.
- **Series catalog (TAG-5).** Migration `0006_tag5_series` adds an empty `series` table to
  `segment_index.db`, mapping each aspect's canonical series keys to their series ids, and
  `weftdb::SeriesId` (`SeriesId::UNTAGGED` is series 0, the empty tag set) names the id.
  It is pure DDL at layout 2: the next open of an existing store applies it, the marker's
  layout and floors stay 2, and no row or frame changes. Nothing registers a series yet;
  tagged writes stay refused.
- **Per-aspect series cap (TAG-7).** `SegmentStoreOptions::max_series_per_aspect`
  (`DEFAULT_MAX_SERIES_PER_ASPECT`, 100,000) bounds how many tag series one aspect may
  register. A batch for a new series past it is refused with the typed
  `StoreError::SeriesLimit` (`400 series_limit` over HTTP) before any frame is written;
  series already registered stay writable, and the untagged series never counts. Tagged
  writes themselves stay refused at both gates until TAG-8 (library) and TAG-11 (HTTP), so
  nothing reaches the cap yet.

- **Series-scoped reads and tagged library ingest (TAG-8).** `SegmentStore::ingest` now
  accepts a non-empty `IngestBatch::groups` and seals it as its own tag series; a group key
  with the reserved `__` prefix is refused with the typed `StoreError::ReservedTagKey`
  (`400 invalid_tag`). Each store read has an `_of` form that takes a `SeriesId`
  (`read_time_range_of`, `downsample_range_of`, `downsample_range_width_of`, `read_point_of`,
  `read_points_of`, `read_value_range_of`, `interpolate_range_of`, `interpolate_range_f64_of`,
  and the `weft-arrow-store` exports), and `SegmentStore::resolve_series` turns a
  `SeriesSelector` into the one series it names, or refuses with the typed
  `StoreError::AmbiguousSeries` (`400 ambiguous_series`) when it matches several. HTTP
  ingest still refuses tags until TAG-11.
- **Stored reads take a series selector (TAG-10).** Every storage data read (`range`,
  `points`, `value-range`, `value-points`, `at`, `at-multi`, `downsample`; 18 routes over
  their formats and verbs) accepts `tag.<key>=<value>` query parameters, matched exactly
  by default and loosely with `tag_match=subset`; `tag.<key>=` requires the tag to be
  absent. No `tag.*` parameter reads the untagged series as before. No match reads empty,
  two or more matches are `400 ambiguous_series`, and a bad, repeated or reserved key or
  more than 16 matchers is `400 invalid_tag`. The JSON read bodies gain a `tags` field; untagged `points`, `value-points`, `at`, `at-multi` and stored `downsample` responses now include `"tags":{}` too, so their bodies are not byte-identical to before (an additive field).
  `SegmentStore::series_tags` reads one series' tags.
- **Tagged ingest over HTTP (TAG-11).** Each ingest request writes one series. The JSON
  body takes `"tags": {"host": "a"}`, CSV and Parquet take `tag.<key>=<value>` query
  parameters, and an ILP payload's own tags name the series. A repeated key (in a JSON
  object or the query), a reserved `__` key, an empty value or anything else outside the
  tag grammar is `400 invalid_tag`; a new series beyond the per-aspect cap is
  `400 series_limit`. Every ingest response gains `series_id` (`0` for the untagged series)
  and `tags` (`{}` untagged), and the `ingest` audit event gains `series_id`.
- **Tag pruning (Phase 4).** Migration `0007_series_time_index` adds
  `idx_segment_index_series_time` on `segment_index(aspect, series_id, min_ts, max_ts)`
  (layout 2, best effort: a store that cannot build it still opens and its series reads use
  the time index). A read of one tagged series seeks that index, bounds its seek by that
  series' own widest segment rather than the whole aspect's, and resolves its selector from
  an in-memory copy of the aspect's series catalog, so it opens only its own series'
  segments however many series share the window. `SeriesSelector::matches_key` tests a
  canonical `SeriesKey` without building a `TagSet`.
- **`WEFT_MAX_SERIES_PER_ASPECT`** (Tuning): the most tagged series one aspect may register,
  default 100,000; a malformed value stops the server from starting.
- **Store read bounds (ROB-13).** The stored downsample and the batch point lookup are
  bounded in `SegmentStore` itself, at 1.0 decision 12's numbers:
  `SegmentStoreOptions::max_downsample_buckets` (`DEFAULT_MAX_DOWNSAMPLE_BUCKETS`,
  1,000,000) and `max_point_batch` (`DEFAULT_MAX_POINT_BATCH`, 10,000 instants). A
  downsample whose fold reaches one bucket more is refused with the typed
  `StoreError::DownsampleTooLarge` while it runs (for a `BucketWidth` the bound is the
  returned, coarsened grid); a wider point batch is refused with
  `StoreError::PointBatchTooLarge` before the index is pruned. Over HTTP both are
  `422 result_too_large` carrying the cap. The single point lookup names one instant
  and needs no cap. The HTTP read ceilings themselves are ROB-12's.

- **Stored-range interpolation route** (Phase 5): `GET /api/v1/storage/{aspect}/interpolate`
  and its `.csv`, `.arrow` and `.parquet` forms interpolate a stored window server side. The grid
  is capped by `WEFT_MAX_INTERPOLATE_POINTS` and the knots by `MAX_KNOTS` (`422
  result_too_large`); the route takes the `read` scope and query admission like the other
  storage reads. JSON and CSV stream their body in chunks as the points are computed; Arrow and
  Parquet are buffered. This ships before 1.0 by an owner amendment to decision 18 (DECISIONS.md).
- **GPU adapter selection and multi-device mode** (experimental, Phase 5.7): `WEFT_GPU_ADAPTER`
  picks the one adapter splimes opens (index, `api:<api>` or a name match) and
  `WEFT_GPU_DEVICES` (`one`, `all` or a list) spreads interpolation over several. `/metrics`
  reports each open device (`weft_gpu_device_info`, `weft_gpu_device_calls_total`,
  `weft_gpu_device_chunks_total`, `weft_gpu_device_in_flight`, `weft_gpu_device_lost`,
  `weft_gpu_device_pool_idle_bytes`). Unset keeps the single-device default; several devices
  measured slower than one in splimes, so it is opt-in.
- **Memory-mapped range reads** (experimental, Phase 5): with the `mmap-reads` cargo feature,
  `WEFT_SEGMENT_MMAP_READS=on` (`SegmentStoreOptions::mmap_reads` in the library) maps sealed
  `.weftseg` frames instead of copying each into a heap buffer. A frame truncated by another
  process while mapped can stop the server with `SIGBUS`, which is why it is off by default.
- **Phase 5 benchmarks**: `weft-bench --gpu-economics` (report schema v29), a device-memory
  soak in `weft-bench --gpu-memory` (`gpu_pool.device_memory`, schema v30), the end-to-end
  interpolation benchmark `weft-e2e-interp` (schema v31) and the GPU conformance matrix
  `weft-gpu-conformance` with tolerances published in `docs/gpu-conformance.md`. Their
  results are not recorded yet.

### Changed

- **splimes 1.3** (Phase 5): WeftDB depends on `splimes = "1.3"` (was 1.0; `pollster` moves to
  1.0.1 with it). Stored interpolation now feeds splimes epoch ticks (`run_epoch`,
  `stream_epoch`) instead of building a `DateTime` per knot.
- **`POST /api/v1/interpolate` range outputs compute in `f64`** (splimes `run_f64`) instead of
  a `BigDecimal` per output point; `/interpolate/point` is unchanged. Results are bit-identical
  below 2^53; above it the kernel's value replaces a narrowing that could be 1 ulp off.
- **The compute endpoints widen JSON `f64` through the shortest round-trip decimal** instead of
  `BigDecimal::from_f64`, so an input of `99.92` is computed as `99.92`, not its binary
  expansion. Non-finite inputs are still rejected.
- **Breaking (HTTP): the storage ILP ingest requires `measurement=` (TAG-11).**
  `POST /api/v1/storage/{aspect}/ilp` now seals only the records of the named measurement,
  and a request without the parameter is `400 bad_request`. This fixes a silent tag drop:
  before, a tagged single-series payload was checked for one tag set and then written to the
  untagged series, its tags discarded. Now those tags name the series the rows land in, and
  two tag sets in one request are still `400 ambiguous_series`. The Rust SDK's `write_ilp`
  sends it from the new `WriteOptions::measurement`; the Python client's `write_ilp` does
  not yet (design PR 7b). The compute routes
  (`/api/v1/{interpolate,downsample}/ilp`) are unchanged.

- **Reads without a series mean the untagged series (TAG-8).** The existing read methods
  answer series 0 only. A store with no tagged writes answers exactly as before; on an aspect
  holding tagged series, they no longer return the tagged rows (use the `_of` reads).
  `StoreError::GroupsUnsupported` is no longer produced and is kept as a reserved variant.
- **Maintenance plans within one series (TAG-6).** Overlap reconciles, squashes, size-targeted
  compaction and the hot/cold reconcile never fold two tag series together; the squash cap,
  the fragmentation gate and `AspectStorageStats::overlapping_segments` (and
  `store_overlapping_segments`) count per series, and each series keeps its own hot tail.
  An untagged aspect is one series, so its maintenance and statistics are unchanged; tagged
  writes stay refused until TAG-8, so nothing observable changes yet.
- **Breaking (library):** `IngestBatch::series_ids()` is removed (TAG-7). It always answered
  `[0]`, and a tagged batch's series id is not known until its commit resolves the key in the
  aspect's catalog; read the id from `IngestReceipt::series_ids` instead. The method shipped
  in `weftdb` 0.2.0 on crates.io, so this is a breaking change, allowed in the next minor
  release by the pre-1.0 policy above.
- **Seal-backed legacy aspects feed a change log, not the per-timestamp queue (S20).**
  Each seal-store commit records one `segment_changes` row (epoch, time span, row count)
  in its own index transaction, and the batch consumer rebuilds the windows a change's
  span touches and acknowledges the row by epoch. Seal-backed ingest no longer writes
  `unbatched_measurements` entries; rows-mode aspects keep the queue unchanged. This
  closes the S20 half of the 0.2.0 "crash consistency is not finished" limitation.
- The `503 unconfigured` sentence no longer names the segment store, since the bench-run
  routes answer it too: it reads "a subsystem this route needs is not configured on this
  server", and `details.subsystem` / `details.enabled_by` say which one and the variable that
  enables it. Match on `code`, as before.
- A maintenance merge (squash, squash to a target size, overlap reconcile) writes its output
  as a paged frame of 4,096-row pages once it holds more than one page, instead of one
  single-block frame. A small window after a squash skips the pages it does not touch rather
  than decoding the segment's value column from its start: on 10M UCI rows squashed to
  65,536-row segments an hour reads in 0.25 ms (0.18 ms before the squash, 0.50 ms with
  single-block output) and the year window in 11.3 ms (13.8 ms before). The squashed store is
  1.357 B/pt rather than 1.341, still below the 1.414 of the 1,024-row seals it replaced.
- A stored read that names no page size is a paged read (ROB-12). It returns this
  server's default page of 10,000 rows — or `WEFT_MAX_PAGE_ROWS`, when an operator
  set it lower — plus `next_cursor`, instead of every row in the window. A client
  that wanted the whole window already had to page through it; a client that named
  nothing asked for unbounded work. `limit` in the response now reports the page
  actually served.

- `weft-server` builds its tokio runtime explicitly (ROB-11, Admission II's runtime-builder
  side): workers are named `weft-worker`, the new `WEFT_WORKER_THREADS` setting sizes the
  pool (unset keeps one worker per available core), and the runtime is shut down with plan
  C9's `shutdown_timeout` of 5 seconds after serving, the task drain and the store's
  shutdown, instead of the implicit `#[tokio::main]` runtime whose drop waits indefinitely
  for blocking work. Admission I's per-class gate budgets and refusals are unchanged.

### Fixed

- CI on `main` is green again after the Phase 5 / W7 merges: test, lint and
  feature-gate fallout fixed (#592, #593).
- A panic inside a legacy queue-consumer write took the worker task with it and
  left the write's transaction open. Every write of the `Inputs` and `Pipeline`
  traits now runs inside `weftdb::exec`'s control-plane write scope (C22,
  ROB-5), so the panic hook added by ROB-2 recognises it, aborts the control
  plane during the unwind, and the process exits with the store's failure code
  instead of dropping a request mid-transaction (#303).
- `batch_capture_measurements` computed a batch's window with `.min().unwrap()`
  and `.max().unwrap()`, which the method's own `# Panics` block documented as
  reachable if the emptiness guard above ever changed. The window is a `Result`
  now, and an empty batch is what it has always returned: no ids, no panic
  (#303).
- An Arrow or Parquet ingest whose `timestamp` column held a null stored that row at the epoch
  (`values()` ignores the validity bitmap); it is now refused `400`, naming the column.
- A stored read with `limit=0` answered a `next_cursor` pointing at the same offset, so a
  client following cursors fetched the same empty page forever; an empty page now carries no
  cursor.
- Parquet ingest (`POST /api/v1/storage/{aspect}/parquet`) refused a Snappy-compressed file
  with `400 … Disabled feature at compile time: snap`. Snappy is the default codec of R's
  arrow, pyarrow, pandas and Spark; the workspace's `parquet` now builds with it.
- **Restore no longer strands tagged frames (part of TAG-9, #310).** A restore-mode open
  adopts an orphan frame that carries a series key into the series the key names,
  registering the key in the aspect's catalog in the same transaction, instead of journaling
  it `held` forever; only a tagged orphan of an undeclared aspect is still held, and a later
  open adopts it once the aspect is declared. Recovery and `POST /api/v1/storage/fsck` also check
  each indexed tagged frame's key: a malformed or repeated key is quarantined suspect, and a
  row whose series id names a different key is rebound to the frame's series, counted in the
  new `RecoveryReport::series_rebound` (also in the fsck response). A row moved back to the
  aspect its frame names gets its key resolved in that aspect's catalog rather than keeping
  the other aspect's series id. Untagged frames and reads are unchanged; a restore's adopted
  orphans now take ids from the commit allocator (which may skip past legacy ids) and their
  aspect's rollup is rebuilt in the same pass.
- **A `NaN` or infinity in an ingest body is refused, not stored as a hole or a
  skipped row (ROB-7).** A Parquet body's `Float64`/`Float32` column turned
  `NaN`/`±Infinity` into nulls on the way through the Arrow converter (a decimal
  has no representation for them, and `None` was the path of least resistance),
  and an ILP line carrying `temp=NaN` was dropped by the projection that could
  not convert it — while the response still answered `rows: 1`, so the sender
  believed the line had been written. All four formats now answer `400 not_finite`
  naming the row (and, for Parquet, the column); JSON and CSV answer it for the
  text spellings (`NaN`, `inf`, `+Infinity`) that a float parser accepts, and an
  empty CSV cell stays a null, which is what an absent measurement means. The
  read side already refused the same values with this code, so it was only ever
  the write side that was lenient.
- Squashing an aspect deleted its merged segments' index rows one statement at a time, and
  under Turso's MVCC each delete cost milliseconds and grew with the swaps before it. One
  guarded statement now deletes a group's rows: squashing 10M UCI rows from 9,769 to 153
  segments takes 31–43 s, not 361 s (694 s was measured on a loaded machine). The index is
  checkpointed after a squash, so later reads no longer walk the deleted rows' versions: an
  hour's read after it costs 0.49 ms, not 2.2 ms (0.22 ms before the squash in that run; what
  remains is decoding into a 65k-row segment) (#507).
- The analytics pipeline's first run interpolates each run of overlapping batch windows once
  and queues the batches 500 to a transaction, instead of one interpolation and one commit
  per window: preparing 10k UCI rows takes 204 s, not ~420 s (#505).
- `analyze_range` read only one step past each 50,000-point chunk, so a gap longer than a
  chunk was interpolated without its far bracket and came out as a single `0`. A chunk now
  widens its read until it holds the knots on both sides (#506).
- **Legacy queue-consumer panics converted to errors (ROB-5, #303).**
  `Pipeline::dictionary()` and `dictionary_mut()` return `Option` instead of panicking when
  no dictionaries are registered, and `build_patterns_queue`'s min/max timestamp extraction
  returns an error with the batch ID instead of `.expect()` on data invariant violations.
  Test coverage added for each conversion.

## [0.2.1] - 2026-11-02

The binaries for the v0.2.0 release. Tagging v0.2.0 built macOS and Windows, but both Linux
builds stopped at the release's glibc-floor check, so v0.2.0 has no GitHub release. This
release is the same code with that check fixed. Only the three binaries (`weft-server`,
`weft-tui`, `weft-bench`) move to 0.2.1. The seven library crates stay at 0.2.0, already on
crates.io, unchanged. Everything in the [0.2.0](#020---2026-11-02) section below, including
its known limitations and the one-way upgrade from v0.1.0, applies to this release.

### Fixed

- The release's glibc-floor self-test read its probe symbols from `/bin/true`, which in the
  `manylinux_2_28` build image is a shell script. It also died without a message when it
  found no symbols, because a failed `grep` under `set -euo pipefail` ended the script.
  The self-test now reads the `bash` binary running it, and finding no symbols reaches the
  script's own error message (#522).

## [0.2.0] - 2026-11-02

The second pre-beta release, cut where roadmap Phase 1 (the benchmark suite) closes. Like
v0.1.0 it ships pre-built binaries on the GitHub release, and it is the first release whose
seven library crates are published on crates.io (`weftdb`, `weft-orchestration`,
`weft-physical-type`, `weft-reduce`, `weft-line-protocol`, `weft-arrow`, `weft-arrow-store`).
WeftDB is still pre-beta: the 1.0 beta bar
(roadmap Phase 7 and W10–W12) is not met, and this release makes no claim that it is.

⚠️ **Upgrading a segment store from v0.1.0 is one-way.** The first open by this
version migrates a v0.1.0 store in place to store layout v2: it writes a
`STORE_FORMAT` marker, adds columns and tables to `segment_index.db`, and from then on
segment ids come from a persistent allocator and maintenance writes new, journaled
frames instead of rewriting them in place (see Added and Changed). v0.1.0 reads no
marker, so it would not refuse a migrated store, but it knows none of this: once this
version has opened a store, do not run v0.1.0 on it again. Before upgrading, stop
`weft-server` and copy the whole store directory; `POST /api/v1/storage/backup` copies
only the control plane, not the segment frames.

### Known limitations

- **Pre-beta.** The HTTP API, the Rust API, the configuration and the on-disk format can
  change in any 0.x release. Nothing is covered by a stability promise before 1.0.
- **Security is new and not yet reviewed.** `weft-server` now serves TLS and enforces API
  keys with roles (see Added), and a non-loopback bind refuses to start without a keys file.
  Keys are read once at startup, so adding or revoking one means a restart; there is no audit
  log; and the security acceptance review the roadmap requires before 1.0 has not been done.
  A loopback bind with no keys file, the default, is unauthenticated. See
  [`SECURITY.md`](https://github.com/basic-automation/weftdb/blob/v0.2.0/SECURITY.md).
- **Crash consistency is not finished.** Seals are write-once and durable, startup recovery
  quarantines damaged frames, ingest is idempotent, and a crash-matrix CI job kills the
  process mid-write. Power-loss durability is so far tested only with the power-cut
  simulator: the privileged power-loss validation on real hardware has not been run. Seal-backed
  legacy aspects still feed a per-timestamp queue rather than a change log (S20), and the
  relaxed-durability mode (S17) is not built.
- **Upgrading a store from v0.1.0 is one-way** (see the warning above), and **v0.1.0 clients
  that send reduction tokens in another case break**: `?agg=Min` is now a `400` (see Changed).
- **The analytics pipeline can miss a long gap's far side** (#506). `analyze_range` works in
  chunks of 50,000 points and reads only one step past each chunk's edges, so a gap longer than a
  chunk (about 35 days at minute resolution) is interpolated without the knot that closes it.
- **The cross-engine figures are one host's.** `bench/reports/phase1-pooled/` pools two runs on
  the project machine, warm reps only, at `small` and `medium` scale; its README says which
  verdicts hold in both runs. Treat a margin under about 1.5× as unsettled.
- **The macOS and Windows binaries are not signed.** Gatekeeper and SmartScreen warn
  before running them.

### Added

- **The pooled cross-engine report** (#508), in `bench/reports/phase1-pooled/`: two full
  `weft-xbench` runs at `small` and `medium` scale against DuckDB, ClickHouse, TimescaleDB,
  QuestDB, InfluxDB 3 Core and IoTDB, pooled with `combine`. 319 of 320 gates pass, and 105 of
  112 verdicts hold in both runs: WeftDB wins 17, all against portable SQL, and loses 85,
  including every load and every comparison with client-side interpolation. The folder's
  README says which verdicts hold and why the one failing gate (TimescaleDB's) is left failing.
- **TLS on the server's listener** (security track SEC-5, #468): `WEFT_TLS_CERT_FILE` and
  `WEFT_TLS_KEY_FILE` (a PEM chain and key, read once at startup) put a rustls handshake
  (ALPN `http/1.1`) in front of every connection on the shared serve
  path. Naming only one of the two is a startup error. A non-loopback plaintext bind starts
  only with a keys file and `WEFT_ALLOW_PLAINTEXT=1`, or with `WEFT_AUTH=disabled`.
- **API keys and Bearer authentication** (security track SEC-6, #472): `weft-server keygen`
  mints a `weft_` key, prints it once and appends only its SHA-256 hash to the keys file
  (`WEFT_KEYS_FILE`, created `0600`, validated whole at startup). Requests carry it as
  `Authorization: Bearer`, compared in constant time, and the route registry's role table
  is enforced: without a key only the probes answer (`401` elsewhere), an unknown key is
  `401` everywhere, a role too low is `403`, and a credential in the query string is
  refused `400 credentials_in_query`. `WEFT_AUTH=disabled` turns it off, `WEFT_AUTH=keys`
  requires the file, and unset means on exactly when a keys file is mounted.
- **`weft-server restore` puts a backup back from the command line** (1.0 plan PAC-13).
  A backup whose restore needs a `main.rs` is no use at 3 a.m., and the server deliberately
  offers no route that touches a live root: `POST /api/v1/storage/restore/drill` rehearses
  into a throwaway directory and stops there. The command reads the backup's `MANIFEST.json`
  and takes the matching leg — `weftdb::restore_store` for a whole-store backup (frames and
  control plane, D-S16), `restore_control_plane` for a control-plane backup — into a
  `--into` root that must not exist yet, so it cannot overwrite a live store. `--adopt`
  opens the restored root once, which is what acts on the restore-intent marker a
  whole-store restore publishes: the frames are adopted into the index in the foreground
  instead of at whatever open first serves them, and `--database`/`--subject` name that
  open's scope (refused without `--adopt`, since nothing else reads them). It prints the
  per-file verify table, rows and bytes, the store UUID the backup carried and what
  adoption did, or the same document as `--json`. A refusal is a status, not a trace:
  `78` when the command line or the destination is wrong and nothing was written,
  `65` when the backup needs the operator — not a backup, a staging name that never
  finished, a backup changed after it was taken, a manifest this release does not know, a
  frame that does not verify, or a store whose write floor this build cannot honour. Both
  codes are the ones the server's startup already returns, so a supervisor reads one
  vocabulary.
- **Every `weftdb` read site is on the frame-reader API** (1.0 plan TAG-4).
  X-FRAME had already routed `SegmentStore`'s production decode sites through
  `weftdb::types::frame_read` (the `read_frame*` readers, with the typed
  `FrameFaultError` under the historical operator sentence); the crate's
  remaining direct uses of the legacy layout-specific readers were in its own
  tests, one of them still choosing a reader from the index row's
  `format_version` copy. Those sites now read through `frame_read` too, so no
  code in the `weftdb` crate names a legacy reader, and two store-level tests
  pin the contract at the public API: a v7-envelope-wrapped frame reads
  through `read_time_range`/`read_point`, and a frame written by a newer
  build fails a store read with a downcastable
  `FrameFaultError { Undecodable { newer_format: true } }` rather than a
  sentence a caller would have to parse. No behaviour changes for any frame
  this build could already read; `weft-bench`'s harnesses still measure the
  legacy readers by name, as their reports say they do.
- **A container image that is proven, not merely built** (1.0 plan PAC-9):
  `packaging/docker/Dockerfile` builds `weft-server` in the same `manylinux_2_28` baseline the
  release binaries use — so the image carries the release's glibc 2.28 floor, read back out of the
  finished image with `scripts/check-glibc.sh` — and runs it on `debian:12-slim` as uid 10001,
  with `/var/lib/weftdb` a volume, `8080/tcp` exposed, the licence set in `/usr/share/doc/weftdb`,
  and no package installed at build time. `scripts/packaging/build-image.sh` builds it, stamped
  with OCI source labels; `scripts/packaging/smoke-container.sh` is what makes the slice more than
  a Dockerfile: it mints keys inside the image with `keygen`, shows that a keys-less `0.0.0.0` bind
  refuses to start, waits on the image's own `HEALTHCHECK` (a bash `/dev/tcp` probe of `/ready`, so
  the image needs no HTTP client), walks the exposure matrix through a loopback-published port,
  declares and ingests a measurement into a named volume as uid 10001, then restarts the container
  and reads the same row back out. `.github/workflows/container.yml` runs the pair on image
  changes, on every push to `main`, and nightly; the scripts themselves are shellchecked by CI on
  every pull request. Nothing is pushed to a registry: the artifact is the proof, and publishing is
  a separate decision. One limitation is documented where it will be read — the health check
  speaks plain HTTP, so a container configured with `WEFT_TLS_CERT_FILE` reads unhealthy.
- **Strict request parsing** (FRE-6b): every request type the server extracts — JSON
  bodies and query parameters alike, on the ingest, compute, storage-read, maintenance
  and backup surfaces — now refuses a field or parameter it does not have, instead of
  silently ignoring it the way serde does by default. A caller with a typo
  (`require_sorte` on an ingest, `limti` on a read) previously got a successful answer
  to a different request than the one it meant to send; it now gets a `400` naming the
  unknown field, in the §4 envelope (`invalid_json` for a body, `bad_request` for a
  query). No in-repo caller sent an unknown field, so none needed updating; the whole
  workspace test suite is the evidence.
- **Startup ordering, exposure matrix, auth-off guard, metrics auth** (security track SEC-8):
  the startup phase list is now named (`startup::PHASES`) and pinned by a test against the
  binary's actual call order. The exposure matrix (plan C8, decision 7) is complete: a
  non-loopback bind with no keys file mounted now refuses to start over TLS as well as
  plaintext (encryption is not authentication), `WEFT_ALLOW_INSECURE_BIND` is dropped for
  `WEFT_AUTH=disabled` (the name is unregistered, so strict configuration refuses it), and
  while authentication is off implicitly a proxied request (`Forwarded` / `X-Forwarded-*`)
  is refused `403 proxied_request_requires_auth` — the gateway deployment
  (`WEFT_AUTH=disabled`) and key-authenticated requests are unaffected. `/metrics` is
  pinned by test to the registry's `metrics` scope when auth is on (decision 17).
- **Admission I** (robustness track ROB-10): the route registry's gate now enforces every
  route's `admission_class`, not only the ingest byte budget D-S14 charged. Probes are never
  shed; query-class requests (reads and stateless compute) are capped in flight by the new
  `WEFT_QUERY_MAX_INFLIGHT` setting (default 128) and shed with `503 overloaded` beyond it;
  write-class requests — declarations included — are charged byte-weighted against
  `WEFT_INGEST_MAX_INFLIGHT_BYTES`; and maintenance-class requests (maintenance and backup)
  are admitted one at a time, a second refused `409 busy` while one runs.
- **Gap fill on the downsample endpoints** (#142): `POST /api/v1/downsample` (body `fill`), its
  ILP sibling and `GET`/`POST /api/v1/storage/{aspect}/downsample` (`?fill=`), in every output
  format, take `null`, `prev`, `linear`, `quadratic`, `cubic` or a constant. Every grid step
  between the bounds is emitted, a synthesized bucket with `count == 0`, a value the method
  leaves out as a missing JSON key, an empty CSV cell or an Arrow/Parquet null; a grid over 10M
  buckets is a `400`. `weft-arrow` gains `reduction_table_with_gaps_to_*`, whose nullable
  columns leave a dense table's schema unchanged.
- **Arbitrary bucket widths** (#143): `weft_reduce::BucketWidth` (`5s`, `15m`, `6h`, `15 minutes`),
  `reduce_width`, `fill_width`, `PartialReduction::coarsen`/`finish_width` and
  `SegmentStore::downsample_range_width` reduce by a whole number of resolution steps,
  epoch-aligned and exact for every reduction (the unit's partials merged). The downsample
  endpoints take a width as the body's `width` or as `?resolution=15m`. TSM-Bench Q5's literal
  `SAMPLE BY 5s` now has a spelling. Additive: every `Resolution` API keeps its meaning.
- **Three interpolation classes in `weft-xbench`** (#148, report v7): beside each engine's portable
  SQL reconstruction (class B), its native fill where it has one (class A: TimescaleDB
  `time_bucket_gapfill` + `interpolate`, QuestDB `SAMPLE BY … FILL(LINEAR)`, InfluxDB 3
  `date_bin_gapfill` + `interpolate`, IoTDB `FILL METHOD LINEAR`, ClickHouse `WITH FILL …
  INTERPOLATE`) and the knots fetched into the client and interpolated there (class C, DuckDB
  included). A native fill computing another answer is gated on the grid's shape and reported with
  its unfilled instants, distance from the line and RMSE, never raced; verdicts are drawn per class.
  The report's command line no longer carries a connection string's password.
- **Each ASOF strategy, run and published** (#393): `clickhouse-double-merge`
  (`join_algorithm = 'full_sorting_merge'`) and `questdb-double-linear` (the `asof_linear` hint)
  beside the defaults; at `tiny` scale QuestDB's hint beat its optimizer's choice.
- **Failure tests** (#147): `weft-bench --ingest --failure kill-ingest|kill-compaction|corrupt-segment|all`
  kills a child process with `SIGKILL` mid-ingest or mid-compaction, or flips a byte of a closed
  segment, and gates the reopened store on exact read-back of every acknowledged row and on a
  damaged segment being quarantined rather than served.
- **Report surfaces** (#149): `weft-bench export REPORT.json --format md|csv|prom` and the run flags
  `--markdown-report`, `--csv-report`, `--prom`; a Grafana dashboard over the Prometheus text in
  `bench/grafana/`.
- **Bench/engine separation check** (#150): `scripts/bench/check-separation.py`, run in CI, fails
  when an engine crate depends on `weft-bench`/`weft-xbench` or reads a `BENCH` variable.
- **`weft-bench sweep`** (#147): `weft-bench sweep KNOB=v1,v2,… -- RUN ARGS` runs a benchmark
  once per value of any run flag, or of the worker pool (`threads`), each into its own directory
  with its own report and manifest, and writes `sweep.json` and `sweep.md`: every result as a
  curve over the knob, with its knee (the first value past which a step gains under 10%
  throughput). On UCI, 2M-row ingest climbs from 109k rows/s at 1,024-row batches to 4.57M at
  262,144 with no knee yet.
- **Seeded query mix** (#147): `weft-bench --ingest --query-mix OPS` replays the soak harness's
  lane model against one store, each operation's lane (ingest a batch, look up a point, scan a
  range) drawn from `--seed` and the operation's index, with `--query-mix-weights` (60,20,20 by
  default), `--query-mix-rows` and `--query-mix-range`. Every read is checked exactly; one report
  per lane, each with a digest of its answers. 3,000 operations over 10M UCI rows passed.
- **Post-compaction reads** (#147): `--compressed-query-compact ROWS` compacts the
  compressed-query store to ROWS-row segments after the measured windows and asks the same seeded
  windows again, as `-compacted` results that must answer digest for digest as before. On 10M UCI
  rows the squash took 694 s (9,769 → 153 segments); every window answered identically, small
  windows read ~36× slower afterwards, and the year window held.
- **`weft-xbench` answer digests, `combine` and run manifest** (#147, report v6): each gate
  carries `answer_sha256` (values to twelve significant digits, so a parallel sum's last bit does
  not count); `weft-xbench combine RUN.json… --out DIR` pools runs with run-first intervals, and
  the renderer calls a win or loss only where every run's median agrees, flagging a report of one
  run as such; the report records its input files' and binary's SHA-256 and, with
  `--container ENGINE=NAME`, each engine container's image digest, command and environment
  (secret-looking variables redacted).
- **`weft-bench combine`** (#147): pools separate runs of one benchmark into one report, with
  intervals from a two-level bootstrap (runs, then samples) so they include the spread between
  runs, and `between_run` per result. Two runs of one build were 41.9% apart at p50, which
  `compare` had called a regression; combined reports of that build compare clean.
- **Result digests** (#147): every `weft-bench` result's correctness carries `result_sha256`,
  a SHA-256 of its canonical output (decimals normalized, so `1.50` and `1.5` agree), and a
  storage estimate whose error exceeds its tolerance makes the result unpublishable.
- **Fair protocol, raw samples and sample counts** (#147): every `weft-bench` result keeps the
  timed samples behind its latency (`samples_ns`, report schema v28), and so does every
  `weft-xbench` result (report v5). `weft-bench verify` now flags a short result (p50 under a
  minute) resting on fewer than 10 samples, an advisory like a dirty tree. The bootstrap gains a
  stddev interval, read off the resamples already drawn, so the existing intervals are unchanged.
  `weft-xbench --reps` defaults to 10 (was 5).
- **Analytics pipeline** (#145): `weft-bench --ingest --analytics-pipeline ROWS` captures the
  first rows of the UCI corpus into a legacy minute-resolution aspect and times the orchestration
  pipeline stage by stage (prepare, patterns, events, correlate, signals), then an incremental
  `run()` after a held-back tail (`--analytics-tail`, `--analytics-batch`), gated on the peak and
  valley detectors finding exactly the extremes the raw rows hold. Report schema v27 adds
  `analytics`.
- **weft-bench records the revision it was built from**: its build script reran only when
  `build.rs` or `Cargo.lock` changed, so a binary built after a commit named the parent commit
  and called its tree modified, and an edit to another crate could leave a modified build
  marked clean. It now also watches git's `HEAD`, index and branch ref and every workspace
  member's sources.
- **Compressed query** (#145): `weft-bench --ingest --compressed-query QUERIES` times
  `downsample_range` inside the store against decoding the window and reducing it, per window size
  from an hour to a year, gated on the two answers being equal. Report schema v26 adds
  `compressed_query`.
- **Online ingest + query** (#145): `weft-bench --ingest --ingest-online READERS` queries a
  store while it is being sealed, compares every query exactly with the corpus, and reruns the
  queries on the finished store as a baseline (report schema v25 `online`). It found that a
  range read's cost grows with the segments the store holds, and that at small batches most
  concurrent reads are refused with `database is locked`.
- **Ingest growth curves** (#145): `weft-bench --ingest --ingest-growth 1M,10M,100M,1B` grows
  one store through those checkpoints over the real UCI corpus laid end to end, reporting each
  stage's seal throughput, per-batch latency, segments and bytes (report schema v24 `growth`),
  each checkpoint gated and written as its own report as soon as it passes.
- **A real smooth-signal corpus for the spline-fill claim** (#144): `scripts/bench/ndbc_to_csv.py`
  turns NOAA NDBC buoy history into a `weft-bench --gf-csv` corpus. On nine buoy temperature and
  pressure series a cubic fill beats linear across short outages and loses across day-long ones;
  README, HONESTY.md and WILLINGNESS-TO-PAY.md now say so.
- **OS-cold and post-restart reps in the cross-engine benchmark** (#146): `weft-xbench
  --cold-hook CMD [--cold-reps N]` times each read phase again after the OS page cache is dropped
  (`os-cold`) and after the engine restarts as well (`restarted`), gated like the warm reps and
  compared only with each other. `scripts/xbench/cold-hook.sh` restarts the reference host's
  engine containers and drops the page cache from a privileged container. The report (schema v4)
  records each result's `warmth` and the hook.
- **Cross-engine reports flag a dirty build**: `weft-xbench` records its source commit as
  `<sha>+dirty` when the checkout has changes, and the report says that commit alone does not
  reproduce the binary.
- **TSBS data in the cross-engine benchmark** (#141): `weft-xbench --tsbs FILE` reads a
  `tsbs_generate_data --use-case=cpu-only --format=influx` file, keeps `host_0`'s `usage_user`,
  and adds `tsbs-tiny`, `tsbs-small` and `tsbs-medium` datasets: knots every ten seconds over the
  synthetic datasets' span, reconstructed per second by every arm and gated against the reference.
  TSBS's tag-grouped query suite is not run; it needs tagged ingest (TAG-12).
- **Apache IoTDB in the cross-engine benchmark** (#413): `weft-xbench --iotdb URL` adds an
  `iotdb-double` arm over IoTDB's table-model REST API (`insertTablet` in time order answered once
  the WAL entry is synced, `lead` segments joined to a `date_bin_gapfill` grid by native `ASOF LEFT
  JOIN`, `date_bin` downsample), gated like every arm; IoTDB has no decimal type, so there is no
  exact-decimal arm. The nightly smoke starts a pinned server.
- **InfluxDB 3 Core in the cross-engine benchmark** (#413): `weft-xbench --influxdb3 URL` adds an
  `influxdb3-double` arm (line-protocol load answered once the WAL is persisted, SQL read back as
  Parquet, window-function interpolation, `date_bin` downsample), gated like every arm; InfluxDB 3
  has no decimal type, so there is no exact-decimal arm. The nightly smoke starts a pinned server.
- **QuestDB in the cross-engine benchmark** (#413): `weft-xbench --questdb host:port` adds
  `questdb-double` and `questdb-decimal` arms over QWP both ways (load timed until the WAL is
  applied, interpolation through the native `ASOF JOIN`, `SAMPLE BY` downsample), gated like every
  arm; the nightly smoke starts a pinned QuestDB server with telemetry off.
- **TimescaleDB in the cross-engine benchmark** (#413): `weft-xbench --timescale URL` adds
  `timescaledb-double` and `timescaledb-numeric` arms (hypertable load by `COPY`, linear
  interpolation in SQL, `time_bucket` downsample), gated like every arm; the nightly smoke starts a
  pinned TimescaleDB server with telemetry off.
- **ClickHouse in the cross-engine benchmark** (#413, first of the remaining external engines):
  `weft-xbench --clickhouse URL` adds `clickhouse-double` and `clickhouse-decimal` arms (load with
  bytes on disk, linear interpolation in SQL, bucketed downsample) against a ClickHouse server,
  gated against WeftDB's in-memory reference like every arm. The nightly smoke run starts a pinned
  ClickHouse server and runs them.
- **First cross-engine report** (evidence track EVI-15), in `bench/reports/evi-15/`: WeftDB
  against DuckDB `DOUBLE` and `DECIMAL` on load, storage bytes, linear interpolation and bucketed
  downsampling, over MCAR, block missingness and the UCI household-power corpus at three scales,
  on the reference SSD. All 99 correctness gates passed. DuckDB loads about 4–27× faster and downsamples
  about 3× faster at `medium`; WeftDB's store is smaller (about half of DuckDB's on the UCI data)
  and its interpolation wins hold only at `tiny` and `small` scale. A second run is kept beside the
  report, and the README records which verdicts held across both. `scripts/xbench/fetch-uci.py`
  fetches the UCI input against its checksum.
- **`HONESTY.md`: four honesty pages** (evidence track EVI-16): when WeftDB is not the right tool
  (with the measured losses: bulk load and large exact-decimal grids against DuckDB, splines on
  prices, the GPU on stored reads), the benchmark methodology (correctness gates, seeds and
  manifests, bootstrap intervals, licence-checked fair comparison), the interpolation-accuracy
  methodology, and precision. The precision page says plainly that interpolated values are computed
  in `f64`; the exact-decimal guarantee covers stored values and reductions.
- **Cross-engine report with a generated honesty section** (evidence track EVI-15R). Every
  `weft-xbench` run now writes `report.md` and a self-contained `report.html` beside
  `xbench.json`, and `weft-xbench render REPORT.json` re-renders one. The honesty section is
  computed from the results: where WeftDB wins and loses (disjoint p50 bootstrap intervals only;
  overlap is reported as noise), correctness failures first, what did not run, known limitations,
  caveats read off the run (disk, `TMPDIR`, debug build, few reps) and how to reproduce it. The
  report schema is v2 (`invocation`, `scratch_dir`, `optimized`); v1 reports still render.
- **`weft-xbench` part II: downsampling, storage bytes, `DECIMAL`, a bulk-load arm and a nightly
  smoke run** (evidence track EVI-14). Every arm now also reduces into epoch-aligned buckets
  (count, min, max, avg), gated against `weft_reduce`, and every load reports its bytes on disk.
  `duckdb-decimal` stores the knots exactly as `DECIMAL(18, s)`, and `weftdb-stored-bulk` seals one
  segment per load, the like-for-like comparison with DuckDB's single transaction. A new
  `cross-engine smoke` workflow builds and runs every arm at `tiny` nightly, failing on any gate.
  Report schema v3.
- **`weft-xbench`: the cross-engine benchmark, WeftDB against DuckDB** (evidence track EVI-13).
  A separate workspace (it builds DuckDB from C++) whose lock is seeded from the workspace's and
  checked for drift in CI. It times **load** (WeftDB seal vs DuckDB appender + `CHECKPOINT`) and
  **linear interpolation** for four arms: WeftDB in-memory, stored, stored `f64`, and DuckDB `DOUBLE`
  via `ASOF JOIN` SQL that follows splimes' extrapolation rule. Datasets are MCAR and block
  missingness at three scales, plus UCI with `--uci`. Every arm is gated against the in-memory grid
  and scored against the ground truth; any disagreement exits 1. Run with
  `TMPDIR=<fast disk> cargo run --release --manifest-path weft-xbench/Cargo.toml`.
- **Stored WeftDB benchmark adapter in `weft-bench`** (evidence track EVI-11). `--stored` adds a
  `weftdb-stored` result to an interpolation run: the dataset is sealed into a temporary
  `SegmentStore` (1024-row segments, the narrowest lossless value type, nanosecond timestamps)
  before timing, and each rep runs `SegmentStore::interpolate_range` over it, so the gap to the
  `weftdb` result is the cost of interpolating on read from storage. The output equals the
  in-memory adapter's point for point. `SystemAdapter` gains an untimed `prepare` hook (no-op by
  default) that `run_profile` and `measure_accuracy` call once before the timed reps.
- **Block missingness and size presets in `weft-bench`** (evidence track EVI-9). `--sparse` is a
  workload of its own: a seeded grid of `--sp-series` series × `--sp-blocks` blocks of `--sp-rows`
  rows is opened cell by cell at `--sp-density` percent, held as one nullable `f64` column per
  series (an Arrow buffer plus its validity bitmap, so an absent block is absent from a column
  rather than an `Option` in a `Vec`), and timed on the block-aligned mean scan that has to visit
  every block because nothing in the column yet says which ones hold values. The report names the
  grid and the **observed** missingness rather than the density asked for — a coin flip per cell
  lands where it lands — and gates cell by cell against the generator's own per-cell means.
  `--scale tiny|small|medium|large` now expands into the size knobs of the workloads that generate
  their own data (`--sparse`, `--gap-fill`, `--downsample`), filling only the knobs left at their
  default and writing the preset into the profile name, so grids of different sizes cannot be
  compared as though they were one run.

- **One HTTP/1.1 serve path with pre-auth listener limits** (security track
  SEC-4, absorbing ROB-23's caps). All serving now goes through
  `weft_server::serve::run` (hyper's HTTP/1.1 server) instead of a bare
  `axum::serve`: a connection cap taken before `accept` (default half the
  process's `RLIMIT_NOFILE` soft limit, clamped to 16..=65,536;
  `WEFT_MAX_CONNECTIONS` overrides), a 30 s header deadline
  (`WEFT_HEADER_READ_TIMEOUT_SECS`), a 30 s keep-alive/read-idle deadline
  (`WEFT_KEEP_ALIVE_TIMEOUT_SECS`) and a 10 s write-stall timeout
  (`WEFT_WRITE_STALL_TIMEOUT_SECS`) — all enforced before any route or auth
  code runs. SIGINT/SIGTERM handlers are installed before every other
  startup phase, so a signal that arrives before serving starts is recorded
  and the server exits 0 without accepting; while serving, a signal stops
  the accept loop and live connections drain for up to 5 s.

- **The `TimeUnit` calendar policy, stated and pinned** (roadmap 4.1). A timestamp column
  holds ticks since `1970-01-01T00:00:00Z` in UTC under the POSIX rule: no zone is stored —
  the column holds instants, so a segment means the same thing whatever the writer's offset —
  and no leap second is modelled: a stored day is exactly `86_400 × nanos_per_unit()` ticks,
  which means the two moments the real calendar inserted a second are represented by counting
  that second twice rather than by a discontinuity or a renumbering. WeftDB carries no
  leap-second table, does not smear, and cannot answer "what was the TAI-UTC offset here"; a
  downsample or interpolation across a real leap second sees the interval as one second long.
  The arithmetic the policy implies moved off the call sites and onto the type:
  `TimeUnit::nanos_per_unit`, `TimeUnit::to_nanos` (checked — a coarse unit near either end of
  `i64` is refused rather than wrapped into a wrongly sorted instant) and
  `TimeUnit::from_nanos` (truncates toward minus infinity, so `-0.5 s` and `+0.5 s` no longer
  share a bucket that a full second either side misses). `scaled_rows` in
  `weftdb/src/types/segment_store.rs` now calls it instead of repeating the per-unit match.

- **Every HTTP route now registers through one sealed registry** (security track
  SEC-3). `weft-server/src/lib.rs` no longer carries the route chain: each of the
  55 routes (49 path templates) is registered in `weft-server/src/routes.rs` by a
  per-area registration function, with a `RouteSpec {scope, audit,
  operator_detail, tier, admission_class, operation_id}`. The scope table is
  frozen at eight scopes (`probe`, `metrics`, `read`, `compute`, `ingest`,
  `declare`, `maintain`, `backup`) alongside six roles with cumulative grants
  (`anonymous` through `admin`); SEC-6 will enforce the table with API keys. The
  one registry produces the router, the policy table, the admission-class table
  and a generated OpenAPI document (`routes::openapi_document()`). The gate is
  fail-closed: a request to a registered path under an unregistered method is
  refused `405 method_not_allowed` with an `Allow` header (previously axum's
  bare 405 with an empty body), and an unregistered path keeps the envelope's
  `404 route_not_found`. No route changed its method, path or handler.

- **`weft-server import-rows-mode` migrates one 0.1.x subject into a Storage v2 store**
  (legacy lane FRE-14c). A 0.1.x data directory keeps one SQLite database per aspect,
  timestamps in milliseconds, with its ingest queue, pattern stores and `metadata.db` catalog
  beside the rows; the server reads such a tree as it is and refuses to open it as a store, so
  carrying one forward meant calling `weftdb::rows_mode` from your own program. The command
  reads one subject, seals its rows into segments, prints what moved and what stays behind with
  the reason for each, and records the run in the store — a second run against the same tree
  refuses rather than doubling the rows. The migrated tree reads back under its own name
  through `WEFT_STORE_DATABASE` and `WEFT_STORE_SUBJECT`.
- **A rows-mode tree too big to hold one aspect in memory, and its catalog's non-row
  state, cross with the rows** (legacy lane FRE-14c remainder). The import no longer
  buffers an aspect: the encoding survey streams its rows through a cursor (one pass
  for the scale and the scale-free candidates, a second for the scaled candidates once
  the scale is known) and the seal streams them again in chunks of
  `--rows-per-segment`, extended past any group of rows sharing the chunk's last
  instant, so a chunk holds at most the chunk size plus one instant group. The encoding
  decided is exactly the whole-column recommendation at tolerance zero, and a tree whose
  `metadata.db` is not the catalog 0.1.x wrote is refused before anything is sealed.
  Numbered migration `0003_fre14_legacy_catalog` registers a `legacy_catalog` table in
  the store's own `metadata.db`, and the import writes one row per aspect it carried —
  the catalog's aspect row (legacy id, resolution, creation time, recorded span,
  compression config) under the store's scope. The catalog's transaction log and host
  paths stay behind, as do the dictionaries and pipeline/correlation/pattern databases:
  1.0 does not replay 0.1.x's log, and the import record in `store_meta` is the
  transaction this migration records.
- **Stage spans for every stored read, and an index-write span on the seal** (roadmap Phase 3).
  Each stored read (CSV, Arrow, Parquet, JSON, point and at-multi) now closes a
  `storage.<stage>.serialize` span tagged with its `format`, so the encode is timed apart from the
  read it follows; the Arrow and Parquet handlers split their encode out of the read to do it. A
  seal's index write closes a `control_plane.index.insert` span carrying `rows_changed`, the
  affected-row count of the insert.
- **A point read or a downsample no longer opens a segment that holds no value**
  (roadmap 4.3). Quality
  pruning is wired into the read path at both levels it applies to. The segment index prune
  drops a segment whose recorded stats say every row is null — `SegmentIndexStore::prune_present_descriptors`,
  the SQL mirror of the resident `SegmentIndex::prune_present_by_time` — so a fully null
  segment costs one index row instead of a file open, a footer read and a timestamp decode;
  inside a surviving frame the same test on the frame's own persisted segment and page stats
  drops pages a point lookup cannot answer. `weftdb::SegmentStore::read_point` and
  `read_points` are the callers, and `weft_physical_type::weftseg::read_frame_point`,
  `read_frame_points` with their single-block and paged forms do the skipping. The answers do
  not change — a null row is not a value — only the work done to reach them.
  `SegmentStore::downsample_range` prunes the same way: a row carrying no value enters no sum,
  count, min, max, first or last, and a partial holding nothing is dropped when buckets merge,
  so a window covered only by nulls yields no buckets rather than a run of empty ones — pinned
  against the single-pass `reduce` over the live rows. `read_time_range` deliberately still
  reads every time-overlapping segment: a null row is part of *its* answer, the timestamps the
  caller asked for, so pruning there would change results rather than cost.
- **Every README performance figure now has a registry entry** (evidence track EVI-2).
  `bench/claims.json` names, per figure, the bench or the local corpus behind it, the date it was
  measured, and whether a clone can re-run it; `tools/check_claims.py` runs in CI and pairs each
  entry with its `<!-- claim:id -->` marker in the README, so a figure added without an entry, a
  figure whose text has drifted away from its marker, a source path that no longer exists, or a
  `*Source:*` line no entry claims all fail the build. Eleven figures are registered: the
  point-read set and `--downsample` are marked re-runnable from a clone, the real-BTC corpus
  figures are marked as not reproducible from a clone, and one entry records a retraction rather
  than a result. No number changed.
- **The Linux release binaries start on a seven-year-old glibc** (portability track
  PAC-2). Both Linux build legs now compile inside the pinned manylinux glibc 2.28
  baseline image instead of directly on the Ubuntu 24.04 runner, so a released archive
  needs glibc 2.28 or newer rather than the 2.34 (`weft-server`, `weft-bench`) and 2.39
  (`weft-tui`) that v0.1.0's did. The floor is asserted, not assumed:
  `scripts/check-glibc.sh` reads the highest `GLIBC_*` version out of each built
  artifact's dynamic symbol table and fails the release above 2.28 — a newer reference
  ships silently and dies at `execve`, which is how this was first noticed (a `fcntl64`
  at GLIBC_2.39 inside a binary built for "Linux"). Measured in the pinned image: 2.28
  for `weft-tui` and `weft-server`, interpreter `/lib64/ld-linux-x86-64.so.2`. The two
  Linux images are now pinned by digest as well as by tag, and
  `scripts/release/test-check-glibc.sh` tests the checker on every push: it runs only in
  the release workflow, which no CI job exercises, and it had landed without its
  executable bit — the step that asserts the floor would have failed with
  "Permission denied" on the first real release.
- **Write poison** (crash-consistency design, S6). When a segment-index COMMIT fails
  in a way that may still have committed (any COMMIT error except a conflict Turso
  detects while validating the transaction, which it rolls back before writing
  anything), the transaction may or may not be durable, so the store now refuses every
  write (seal, declare, reconcile, split, merge, squash, compact, rollup rebuild) with
  the new `weftdb::Poisoned` error until the process restarts, while reads keep
  working; the restart's open settles the transaction. `SegmentStore::poisoned()`
  reports it, and `GET /ready` gains `poisoned`, `restart_required` and
  `poison_reason`. `WEFT_ON_AMBIGUOUS_COMMIT=exit` makes `weft-server` log and exit
  with status 70 once its store is poisoned, for deployments whose supervisor restarts
  it (unset or `poison` keeps the default); the library itself never exits the process
  (see the options entry under Changed). `SegmentStore::wait_until_poisoned()` and
  `subscribe_poison()` let an embedder do the same.
- **A panic during a control-plane write ends the process** (robustness track ROB-2, release
  plan C12, C22). `weftdb::exec::control_plane_write` already marks the region that the whole
  `segment_index.db` write path runs in; the server now acts on it. `weft_server::panic_hook::install()`
  is installed at startup immediately after the tracing subscriber, chained to the hook it
  replaces so the panic is still reported with its message and location. A panic whose
  panicking task is inside that region is written to stderr and the process
  `std::process::abort()`s: the exit carries a signal a supervisor sees (SIGABRT) instead of
  an HTTP status, and the restart that follows reopens the store under the engine's own crash
  recovery, which is the component that can tell whether a commit was torn. A panic anywhere
  else is reported and left unwinding, because wave 6's ROB-3 catch layer is the only layer
  that catches panics. The hook does not write the poison marker — most of the region reads
  before it writes, so a marker there would turn a benign panic into an outage lasting until
  an operator deletes a file. `weft-server/tests/control_plane_panic.rs` proves both halves
  by re-executing the test binary as a child, the way `poison_exit.rs` does.
- **Store format marker and migration registry** (freeze design §4.3, FRE-12a). A
  segment store root now carries a `STORE_FORMAT` file (plain JSON: `layout_version`,
  `min_read_layout`, `min_write_layout`, `last_written_layout`, `migrating_to`,
  `applied_through`, `store_uuid`, `scope`), written through the durable I/O layer
  (temporary file, fsync, rename, directory fsync). The open reads it right after it
  takes `LOCK` and before it opens any database, and refuses a store whose
  `min_write_layout` is newer than this build's layout (`weftdb::SUPPORTED_LAYOUT`,
  now 2) with `weftdb::StoreError::IncompatibleLayout`, having touched nothing but
  `LOCK` and `LOCK.holder`; an unparseable marker is `StoreError::UnreadableStoreFormat`.
  A new store writes its marker before any database file exists. Every control-plane
  table is now created by a registered, idempotent migration (`0001_baseline`: the
  pre-1.0 schema; `0002_s6_s7`: layout 2), recorded in a new `store_migrations` table
  of `segment_index.db`; the open runs every pending migration's precheck first, raises
  the marker's floors before it applies one, so a crash half way never leaves floors an
  older WeftDB would not respect, and mirrors the marker into `store_meta`. A root
  without a marker but with databases is judged by `store_meta` before anything is
  written to it: a newer write floor recorded there is `IncompatibleLayout`, a lost
  marker is rebuilt from that copy (keeping the store's layout, floors and identity),
  and a store written before markers existed (any v0.1.0 store) is layout 1 and is
  migrated in place, one way (see the upgrade note at the top of this section). A
  store whose layout is newer than this build's but whose write floor it meets opens
  without migrating (recovery is told to only report); a root holding only such a
  marker, with no database, is refused with `StoreError::NewerStoreWithoutDatabases`
  and nothing created. No open function runs DDL any more, and the control-plane
  databases are opened only by a store (see Changed).
- `SegmentStore::open_report()` (`weftdb::OpenReport`: `created_new`,
  `migrated_from`, the migrations `applied`, `recovery_report_only`),
  `SegmentStore::store_uuid()` and `SegmentStore::store_format()`. `weft-server`
  prints whether it created, opened or migrated its store, with the store's UUID and
  layout.
- `segment_index.series_id` (`INTEGER NOT NULL DEFAULT 0`, 0 being the empty tag set)
  and `aspect_seq.next_series_id` (`DEFAULT 1`), in migration `0002`, so tagged
  series need no table rebuild later (tags A7). Every row reads back as series 0.
- `weftdb::exec::control_plane_write` and `weftdb::durable::poison_global()`
  (robustness track ROB-2): every segment-index transaction runs inside the
  task-local control-plane write scope, so a panic hook can tell a panic in the middle
  of a control-plane write from one in a read. `poison_global()` is a process-wide
  poison that, once set, refuses every write of every store, as a store's own poison
  does, while reads go on. It refuses every store open too (`weftdb::Poisoned`), since
  an open writes.
- `LOCK.holder` records the holder's host name and boot id beside its pid, and a
  second opener's error names the host (`… in use by pid 1 on host db-2 (session …)`).
  The short wait for a lock that a child being spawned still shares now applies only to
  this process on this host in this boot: another container on the same volume, also
  pid 1, is refused at once.
- On Apple targets every control-plane connection sets `PRAGMA fullfsync=ON`, and the
  open checks it beside `synchronous=FULL`: Turso syncs a COMMIT with plain `fsync`
  there otherwise, which does not survive power loss.
- `Database::list_stored_databases()` lists the legacy databases on disk (the folders
  of the data directory that hold a `metadata.db`), sweeping the build directories of
  interrupted `Database::new` calls first. `weft-tui` lists databases through it.
- `Outputs::get_unbatched_entries` and `Inputs::dequeue_unbatched_entries` read and
  dequeue unbatched-queue entries together with their `queued_at` (`UnbatchedEntry`),
  so a queue consumer dequeues only what it read; see the ingest fix below.
- `Inputs::remove_extracted_batches` removes the processed batches pattern extraction
  consumed and records their hashes, so the batch consumer does not queue them again
  while the record keeps them; `build_patterns_queue` uses it (see the batch dedupe fix
  below).
- `WEFT_EXTRACTED_BATCH_RETENTION_SECS` (weftdb): how long, in seconds, the record of
  extracted batches keeps a batch's hash. A positive whole number; anything else keeps
  the default, 48 hours. Set it above the longest interval between pipeline runs of an
  aspect plus the longest ingest call (see the batch dedupe fix below).
- `Database::get_earliest_measurement_uncached` reads an aspect's earliest measurement
  from its rows, bypassing the per-instance cache; the incremental build aligns its
  windows on it.
- `weft-orchestration` has a `fault-injection` feature (it enables weftdb's) for its
  crash tests: `cargo test -p weft-orchestration --features fault-injection --test legacy_queue_crash`.
- `weft-server` has a `fault-injection` feature too (it enables weftdb's), for the test
  that `GET /ready` reports a poisoned store:
  `cargo test -p weft-server --features fault-injection --test ready_poisoned`. Under
  that feature `SegmentStore` has a hidden `inject_poison` test hook.
- `MaintenanceBusy`, `MaintenanceWait`, `DEFAULT_MAINTENANCE_WAIT`,
  `SegmentStore::maintenance_wait` and `SegmentStore::with_maintenance_wait` (see the
  maintenance entry under Changed), and `SegmentStore::index_conflicts()`, the number of
  segment-index transaction attempts that lost an MVCC conflict since the store opened,
  which stays at zero unless something outside the store contends for the index. Under
  the `fault-injection` feature `SegmentStore` has a hidden `hold_maintenance` test hook,
  used by `cargo test -p weft-server --features fault-injection --test maintenance_busy`.
- `SegmentStore::reap(MaintenanceWait)` (`weftdb::ReapSweep`): runs the reaper of the
  frame journal (see the write-once maintenance entries under Changed) over every aspect
  with journal rows, unlinking each frame a maintenance operation retired once no
  running read and no backup may still need it, and each output an operation journaled
  but never swapped in. Every maintenance operation reaps after itself, and every open
  replays the whole journal, so this is for frames a long read still held, that the
  filesystem asked to retry later, or that an operation stopped part way (its future
  dropped, a request cancelled say) left behind; the next maintenance operation on such
  an aspect also settles those first.
- `weftdb::aspect_name::encode` and `decode`: the file-name form of an aspect name in the
  write-once frame names (crash-consistency design §4): `[a-z0-9_]` as they are, every
  other byte as `%XX` in uppercase hex, injective even where the filesystem folds case
  or normalises Unicode.
- **Series tags: `TagSet`, `SeriesKey` and `SeriesSelector`** (B-tags, TAG-1), in the
  new `weft_physical_type::tags` module and re-exported by `weftdb` (also as
  `weftdb::tags`). Nothing stores or reads tags yet; this is the one validated form
  that frames, the control plane, ingest and reads will share. The canonical form and
  its caps are **frozen format**:
  - a key is 1–128 bytes, starts with an ASCII letter or `_` and continues with ASCII
    letters, digits, `_`, `.` or `-`; the prefix `__` is reserved for system
    dimensions;
  - a value is 1–256 bytes of UTF-8 with no C0 control character (U+0000–U+001F) and
    no U+007F, compared byte for byte (no Unicode normalization);
  - a tag set holds at most 16 tags, with no key twice. An empty value or a repeated
    key is an error, never dropped or merged;
  - the canonical key (`SeriesKey`) is the tags sorted bytewise by key, each written
    as key, 0x1F, value, joined by 0x1E, and is at most 1,024 bytes. The empty set is
    the empty string and names series 0. Neither separator can occur in a key or a
    value, so the form is injective without escaping, and a frame's binding to its
    series will be a byte compare.

  `TagSet::from_pairs` takes pairs in any order and rejects every rule break with a
  typed `TagError` that names the key and the limit; the enum and each of its variants
  with fields are `#[non_exhaustive]`.
  `SeriesKey::from_canonical` reads stored bytes back and rejects bytes that are
  unsorted, repeat a key, break a rule (non-UTF-8 values included) or exceed the cap.
  Unlike input, it accepts a reserved `__` key, so that a frame a later version writes
  with a system dimension stays readable. `SeriesSelector` holds equality matchers
  joined by AND, at most one per key, where `None` means "tag absent".
  `SeriesSelector::new` selects every tag set the matchers hold for;
  `SeriesSelector::exact` also requires that the tag set has no other tag. With no
  matchers they are `SeriesSelector::all` (every series) and `SeriesSelector::untagged`
  (series 0 only). The caps and separators
  are public constants (`MAX_TAG_KEY_BYTES`, `MAX_TAG_VALUE_BYTES`, `MAX_TAGS`,
  `MAX_SERIES_KEY_BYTES`, `TAG_KEY_VALUE_SEPARATOR`, `TAG_PAIR_SEPARATOR`,
  `RESERVED_TAG_KEY_PREFIX`).
- **Integer-native exact-decimal reductions** (`weft-reduce`). `reduce_scaled` and
  `reduce_partial_scaled` reduce `ScaledI64` mantissas at one scale, with epoch-nanosecond
  instants, in integer accumulators and build one `BigDecimal` per bucket; their buckets
  equal `reduce` / `reduce_partial` on the same data. `reduce_scaled` covers `min`, `max`,
  `avg`, `sum`, `first` and `last`; `reduce_partial_scaled` also takes the `sketch_p*`
  reductions, with identical sketches. Both return `Ok(None)` for a reduction they do not
  cover, so the caller falls back.
- **Physical-value and batch reads** (`weft-physical-type`).
  `weftseg::read_segment_range_physical` and `read_paged_segment_range_physical` return
  each present value as its stored `PhysicalValue` (for `ScaledI64`, the mantissa and
  scale) instead of a `BigDecimal`. `weftseg::read_values_at` reads many indices of a
  value block in one walk of its block chain, through the new
  `timestamp::{blocked_bitpack_decode_gather, for_bitpack_decode_gather}` (and, with
  `bitsliced-codec`, `transpose_bitpack_decode_gather`). The blocked and bit-sliced ones
  decode a block whole once `timestamp::GATHER_WHOLE_TILE_THRESHOLD` requests land in it
  and value by value below that.
- **Advisory size estimates for three codecs not written to disk** (`weft-physical-type`):
  the decimal-exponent FOR value codec, which factors each block's shared power of ten out
  before FOR packing (`timestamp::dfor_bitpack_{bytes,encode,decode,decode_range}`,
  `ColumnEncoding::dfor_value_bytes`), and two timestamp-column estimates,
  `DeltaOfDeltaColumn::common_multiple` / `common_multiple_estimated_bytes` (the deltas'
  common factor taken out) and `delta_for_estimated_bytes` (first-order deltas under
  per-block FOR). They are not behind `experimental-codecs` and have not had the patent
  review the gated codecs are waiting on.
- **Weft-Bench runs its storage workloads on a real CSV corpus.** `--comp-csv`,
  `--pl-csv`, `--rf-csv` and `--ds-csv` feed the compression, point-lookup, range-fetch
  and downsample workloads from a CSV file instead of a generator, with
  `--csv-value-col` (0-based value column) and `--csv-skip` (data rows skipped after the
  header). Reports gain `storage.advisory_dfor_value_bytes` (bench schema v17),
  `storage.advisory_common_multiple_timestamp_bytes` and
  `storage.advisory_delta_for_timestamp_bytes` (v18), each present only when it beats the
  realized codec, and `metadata.work_disk_kind`, `work_disk_file_system` and
  `work_disk_mount_point`, the disk under the working directory (v19).
- **Weft-Bench's downsample generator emits two-decimal values** (`--ds-decimals`, default 2;
  `full` keeps the previous generator's exact binary expansion of each float). The old values
  carried ~50 significant digits and made the published reduction figures 7.3× slower than real
  data; the README table is re-measured with both generators, and its claim that `sketch_p99` is
  ~2.6× faster than exact `p99` is withdrawn (level on two-decimal values, 1.36× on real BTC).
- **Gap filling** (`weft-reduce`): `weft_reduce::fill` turns a reduction's buckets into the
  dense grid between two bounds, keeping measured buckets unchanged and synthesizing every
  empty step with `count == 0` by a declared `Fill` (`Null`, `Previous`, `Linear` by grid
  step, a constant `Value`, or `Spline`, a splimes spline through the bucket values;
  `Fill::from_token` parses `null`/`prev`/`linear`/`quadratic`/`cubic`/a decimal),
  bounded by a caller-given bucket limit (`FillError`).
- **Weft-Bench `--gap-fill` workload**: TSM-Bench Q5's `SAMPLE BY … FILL(LINEAR)` shape over
  a seeded series with outages (`--gf-points`, `--gf-stride`, `--gf-bucket`, `--gf-outage`,
  `--gf-outage-len`, `--gf-fill`, `--gf-agg`), gated on a dense grid and scoring the filled
  buckets against the clean signal; `--gf-csv` cuts the outages from a real series and
  scores against the real values removed.
- **Weft-Bench `--gpu-memory` workload**: repeated mixed-size GPU interpolations with the
  buffer pool's statistics sampled after every call, gated on bounded, non-growing idle
  memory (`--gm-rounds`, `--gm-sizes`, `--gm-knots`, `--gm-pool-mib`); reports carry a
  `gpu_pool` block (bench schema v22).
- **Weft-Bench reports the first rep apart from the rest**: `cold_warm` (bench schema v21)
  carries the first timed rep and the warm reps' latency statistics for every result with
  two or more reps; `latency` still covers every rep.
- **Weft-Bench `--ds-scaled`**: the downsample workload times the integer-native reduction a
  stored `ScaledI64` aspect takes (`reduce_partial_scaled`), gated on matching `reduce`.
- **Weft-Bench `--decimals N`** for `--synthetic` (`SyntheticParams::value_decimals`):
  rounds the generated samples to N places. The default keeps each sample's exact binary
  expansion, which interpolates ~2× slower at 100k samples than two-decimal input.
- **Weft-Bench `--parquet`**: the results as a flat Parquet table beside the JSON report, one
  row per result with the run metadata repeated, for querying many runs at once.
- **Weft-Bench records the GPU driver.** An interpolation run's `metadata.engine` gains
  `gpu_driver`, the driver's name and version as splimes' `GpuInfo::driver` reports it
  (e.g. `NVIDIA 610.57.04`), beside the adapter in `gpu`; it is printed and shown in the
  HTML report, and omitted when the driver reports nothing (bench schema v20).
- **One error envelope and one code table for every refusal** (1.0 plan X-ERR; absorbs SEC-2,
  FRE-6a, ROB-1 and TAG-0's envelope). Every refusal `weft-server` sends — a route guard, a body
  or query extractor, or a library failure behind `anyhow` — is now
  `{"error","code","request_id"}` plus optional `details` / `limit` / `committed`, as JSON on
  every path, including the ones that answered axum's empty or plain-text 400 and 404 bodies.
  The code is one of 42 named by `Code` in `weft-server/src/error.rs`, whose table is asserted by
  name and status against the plan's §4 table. Refusals are classified by **type**: `anyhow`
  downcasts find `weftdb::InvalidAspectName`, `weftdb::StoreError`,
  `weft_physical_type::SealError`, `weft_reduce::ReduceError` and `weft_reduce::SketchError`, and
  the seal wrappers in `SegmentStore` now keep the typed error as the failure's source while
  keeping their own `seal failed: …` sentence as the message, so rewording a wrapper cannot move
  a status. Two message rules remain, each labelled with the slice that retires it: `no declared
  schema` until D-S13 makes an absent schema a typed error, and `weft-arrow-store`'s untyped
  `require_sorted` bail until the ingest lane types it (D-S10/D-S14). A `5xx` now names only its
  condition — `internal`, `commit_failed`, `frame_undecodable`, `series_binding_mismatch`,
  `incompatible_store`, `poisoned`, `outcome_unknown`, … each with one fixed sentence — and its
  cause goes to the log, so no driver or library message reaches a client carrying a path, a
  table name or an aspect name (SEC-2). `axum`'s `CatchPanicLayer` is dropped: a panic is a
  process death with the exit code the supervisor sees, not a response body (FRE-6a).
  `ConvertError`, `ReduceError` and `SketchError` gained `#[non_exhaustive]`, so the mapping can
  grow without breaking a semver-minor release.
- **An aspect name is checked twice — at creation and on use** (1.0 plan X-NAME; absorbs FRE-7a,
  SEC-0). `weftdb::aspect_name::validate_new` adds the rules that only a name not in use yet can
  still be held to: no ASCII uppercase letter, because `price` and `PRICE` are one and the same
  file name on a case-insensitive or case-folding filesystem (NTFS, APFS by default, ext4 with
  `casefold`), and the legacy `<aspect>-<id>.weftseg` scheme writes the aspect name into that name
  literally — two aspects, one file, and the second one's frames are read as the first's. Only
  `POST /api/v1/storage/aspects` applies it, because declaring is where a name is born: reads,
  seals, maintenance and the store-wide sweep keep plain `validate`, so a store that already holds
  `Price` is still readable, writable and compactable. A refusal carries
  `code: "invalid_aspect_name"` and `details.reason`. The characters only Windows forbids
  (`< > : " | ? *`) are deliberately **not** refused at creation on Unix while the aspect name is
  still part of the file name — `job:rate5m` is a name in use — and that cross-platform hazard is
  closed one layer down instead, where `aspect_name::encode` escapes `:` for the write-once frame
  names (D-S10).
- **Our executables say what allocator they run on** (1.0 plan PAC-3). `weft-server` and
  `weft-tui` each declare `#[global_allocator] static ALLOCATOR: mimalloc::MiMalloc` in their own
  `main.rs`, through a direct `mimalloc` dependency rather than one inherited from a library.
  Same allocator, same version, same measured behaviour — the decision is just made in the place
  that owns the process (see the dependency entry under Changed).
- **Frame versions are a registry, and a frame says what it is** (1.0 plan X-FRAME; absorbs FRE-4,
  TAG-2 and ROB-8's fault-class naming). `weft_physical_type::frame_registry` names the frame
  layouts — `5` single-block, `6` paged, `7` envelope — on the rule stated there that a number is
  never reused or renumbered once it has shipped. The v7 envelope is a header and a trailer around
  a frame stored **verbatim**: `WEFTSEG | u16 7 | u16 body_version | extension table | frame |
  crc32`, 16 bytes of overhead (a 12-byte header plus the trailer), so a v5 or v6 body keeps every
  byte-for-byte expectation a released server already holds while a reader gains somewhere to put
  material in front of a body it is not allowed to change. An extension is `{id, critical,
  payload}`: a reader that does not know a **critical** one refuses the frame, because that body
  was written on the assumption that the extension would be applied, and an optional one is
  ignored. The set this build implements (`KNOWN_EXTENSIONS`) is deliberately empty — the envelope
  ships with no extensions, and a number invented before there is a feature behind it can never be
  taken back. `describe_frame`, `frame_layout_of`, `wrap_frame_envelope`/`unwrap_frame_envelope`
  and the layout-aware readers `read_frame_segment`, `read_frame_paged_segment`, `read_frame_point`,
  `read_frame_points`, `read_frame_range` and `read_frame_range_physical` read a frame without the
  caller knowing how it is framed, and `frame_fault_of` gives the class an operator acts on (plan
  C5): `Integrity`, the bytes are damaged, or `Undecodable { newer_format }`, this binary is behind
  the file. `SegmentStore` picks its reader from the frame's own header rather than the
  `format_version` it recorded in the index row, where any value that was not `6` used to send a
  paged frame into the single-block decoder; the decode failures it returns carry
  `weftdb::FrameFaultError` in the `anyhow` chain under the `decoding [paged ]segment <path>: …`
  sentence they have always produced. Over HTTP that is C5: a frame this build has no reader for is
  `503 incompatible_store`, and any other frame that will not decode is `500 frame_undecodable`,
  the code X-ERR gave a wire sentence but no producer. **No writer opts into the envelope yet** —
  seals keep writing 5 and 6 — so this is the mechanism, not a format change.
- **A WeftDB 0.1.x data directory is refused, not quietly emptied** (1.0 plan FRE-14, first half).
  A 0.1.x root keeps one database per aspect — `metadata.db` at the top and
  `<subject>/<aspect>/measurements.db` two levels down — and that `metadata.db` is one of
  `weftdb::CONTROL_PLANE_FILES`, so opening such a root used to **succeed**: it found a control
  plane, created the three databases it was missing, added `segments/`, wrote a `STORE_FORMAT`
  marker beside the old data, and reported an **empty** store with no error anywhere. Before any
  database is opened, `SegmentStore::open` now recognises the shape and returns
  `StoreError::RowsModeDataDirectory { aspects }`, naming each `<subject>/<aspect>` pair in path
  order. It rides X-ERR's existing mapping — `503 incompatible_store`, CLI exit 65 — so there is no
  new status code. The refusal writes nothing but `LOCK` and `LOCK.holder`: the test asserts the
  file set is otherwise unchanged, that no marker is added, and that a second open refuses the same
  way. Turning those rows into frames is the second half of FRE-14, below.
- **A WeftDB 0.1.x data directory can be read into a store** (1.0 plan FRE-14, second half).
  `weftdb::rows_mode` reads the rows-mode tree the refusal above names. `discover` reports the
  series under `<subject>/<aspect>/measurements.db` — row count, span, how many rows share an
  instant — and lists, with a reason, everything it will not carry: the `metadata.db` catalog,
  the databases 0.1.x kept beside the rows, and the `dictionaries/` directories. An aspect's name
  comes from its **directory**, not from the database: `measurements.db` holds only
  `(id, dataset_id, timestamp, value)` — 0.1.x's own DDL (`weftdb/src/types/aspect.rs` in the
  `v0.1.0` tag) — so the directory is the only record of what a series is called. The value column
  has to be the `value` that DDL created: a database that names it after the aspect is refused, not
  read, because an aspect called `dataset_id` would otherwise be read out of the column holding its
  identifiers. `import_into` reads
  one subject into the store scoped to that `(database, subject)` pair, declares each aspect under
  the narrowest encoding that holds its values exactly (tolerance zero, `recommend_encoding` — the
  reading trades no precision across the version boundary), seals the rows in batches
  (`rows_per_segment`, default `DEFAULT_ROWS_PER_SEGMENT`) without ever splitting a group of rows that
  share one instant across two segments — the store folds two segments whose windows intersect and
  keeps the newer row at an instant they share, so a group cut in two would lose rows in a compaction
  the reading does not control; a segment can therefore come out a few rows longer than asked. The
  reading then writes an `ImportRecord` into the
  store's own index under `rows_mode_import:<database>:<subject>`; `read_import_record` reads it
  back after a reopen. That record is what stops the same tree being read twice
  (`RowsModeError::AlreadyImported`), because after the fact a migrated row and a row the store
  sealed itself are indistinguishable — so an aspect that already holds segments refuses first
  (`AspectNotEmpty`), as does a tree whose database directory is not the store's database
  (`ScopeMismatch`: 0.1.x puts its database only in the path, so the path is the only thing that
  can say where a tree belongs). Timestamps are `Millis` and cross unchanged: 0.1.x stamped
  `timestamp_millis()` into an `INTEGER`, and the `resolution` its catalog records is the sampling
  grid, not a unit, so there is nothing to infer and nothing is inferred. A value cell that is not
  the text of a decimal stops the read and quotes the row (`ValueNotDecimal`) rather than putting
  a number nobody can vouch for into the store. The tree keeps its bytes: each `measurements.db`
  is opened bare, not through the control plane's `connect`, which switches journal mode and so
  writes; the test compares every file it found, byte for byte, after the reading. The aspect list is
  written one aspect at a time and its record last, so a reading that stops partway leaves the aspects
  that already crossed in the store with nothing recorded — the next attempt refuses on `AspectNotEmpty`
  rather than finishing the job, and saying what an operator does then is the remaining part of FRE-14:
  the route an operator runs from the command line or the server, the numbered migrations that register
  `metadata.db`'s schema, and a reading of a tree too big to hold one aspect in memory.
- **A checker for the metadata a release is made of** (1.0 plan FRE-1). `scripts/crates/check.py`
  reads the workspace manifests and reports, one line per problem, each state that would make a
  publish fail or ship the wrong thing: a crate marked `publish = false` that is not one of the three
  binaries the release archives, or one of those three without it; a crate that declares no version of
  its own; an `edition` a crate sets against the workspace's; an internal path dependency with no
  version requirement, or one the member's own version does not satisfy; a published crate with no
  `README.md`, no `documentation` URL, or a `license` expression whose text is not in the crate
  directory. It reads the manifests and nothing else — registry resolvability cannot be checked before
  the first publish, because `cargo package -p weft-reduce` needs `weft-physical-type` to already be on
  crates.io, which is the chicken-and-egg `scripts/publish.sh` documents — so it runs offline: in CI as
  the new `crates` job, and again inside `publish.sh` before it uploads anything. Its `publish-order`
  subcommand prints the publishable members in dependency order, topological with member order as the
  tie-break, and `publish.sh` takes both its list and its order from it rather than from an array
  someone has to remember. `scripts/crates/test_check.py` (36 tests over a throwaway four-crate
  workspace) includes the one that runs the checker against this repository.
- **Committed goldens for the bytes the store writes** (1.0 plan FRE-3). Every frame-format test in this
  repository was a round trip — encode, decode, compare against the input — which passes unchanged after a
  change that moves the writer and the reader together, so it said nothing about whether a reader from the
  previous release can still read a file this one writes. Three suites now diff against committed bytes:
  `weft-physical-type/tests/golden_frames.rs` over 20 `.weftseg` fixtures (single-block v5 across every
  reachable physical type and value codec, all five timestamp block codecs, the persisted checkpoint index,
  the paged v6 layout with its page table, the v7 envelope bare and carrying an unknown non-critical
  extension, and the bit-sliced transposed codec behind `bitsliced-codec`); `golden_codecs.rs` over 40
  encoding fixtures — 36 byte images plus four text records, because a run-length split or a
  codec-selection table is easier to review as words than as hex (the bit-packed width and payload, the
  run-length split, the delta-of-delta anchors, the zigzag varint sizing and, behind
  `experimental-codecs`, each persisted `f64` codec over the corpora that realize it plus the fixed table
  the codec-selection heuristic chooses from); and
  `weftdb/tests/golden_sidecar.rs` over 2 `.weftpart` fixtures, a base resolution and a rollup chain. Each
  case pins two things: the bytes, and the contents those bytes decode to, against a result recomputed from
  the corpus — so a fixture re-recorded after a behaviour change cannot smuggle different numbers through
  the byte pin. A mismatch reports the offset, a hex window around it, and what a reader from the last
  release would now make of the file; `WEFT_UPDATE_GOLDENS=1` re-records, and the failure text says plainly
  that re-recording is a compatibility decision rather than a test fix. An orphan check fails if a fixture
  appears in `tests/golden` that no case writes, or a case writes one that is not committed, so the fixture
  set cannot drift from the suite behind it. Gated fixtures stay on disk in a build without their gate and
  are skipped rather than deleted — their presence is the gate working. The gated halves are exercised by
  CI's `features` job, which builds `weft-physical-type --all-features` (plan note C21 asked for exactly
  `--features experimental-codecs,bitsliced-codec`, which that job's `--all-features` is a superset of).
- **An open value-provenance vocabulary** (1.0 plan TAG-3). `weftdb::ValueKind` names how a value in a
  result was produced — `raw`, `interpolated`, `extrapolated` — as a type a client can match on without
  the match becoming a breaking change later. splimes' `PointKind`, which the compute endpoints
  re-export as the JSON `kind` field today, is an exhaustive enum, so WeftDB cannot grow it in front of
  its own clients: code compiled against it today stops compiling in the release that adds a kind.
  `ValueKind` is `#[non_exhaustive]`, and it is open now rather than only in some future release — a
  token this build cannot name parses to `ValueKind::Unknown` holding the token verbatim, so a newer
  vocabulary read by an older client degrades to "a kind I have not been told about" instead of a parse
  error or a wrong arm. The wire tokens are the compatibility surface, and a test pins them to the three
  strings the endpoints already send; `RESERVED_VALUE_KIND_TOKENS` (`compressed`, `reconstructed`)
  reserves those token names before either becomes a variant, which is what lets a client special-case
  them now and still compile when they arrive. `From<PointKind>` is total (splimes' kinds are a subset),
  `TryFrom<ValueKind> for PointKind` fails with the token it could not express, and the `serde` impls
  carry the token rather than the variant. No read path emits a `ValueKind` yet: the lanes that report
  provenance adopt it where they already know it (TAG-0, FRE-5, FRE-6b, ROB-16), so until then adding
  the type costs an existing caller nothing.

- **A run manifest, so a benchmark number can be re-checked** (1.0 plan EVI-3). A Weft-Bench run wrote a
  report of numbers and a machine fingerprint, and nothing tied the two to the artifact that produced
  them: a quoted figure carried no proof of which build, which toolchain, or which file it came from, so
  a claim about a benchmark could not be audited or reproduced from the report alone. Every run now also
  writes `<report-stem>.manifest.json` beside the report (`--no-manifest` opts out): schema v23, and its
  own schema — the report artifact's schema stays at v22, because the numbers did not change shape, only
  the record around them did. It carries the toolchain facts of *the binary that ran*: `rustc` semver,
  commit and host, LLVM version, target, and the profile the code was optimized with (`profile`,
  `opt-level`, `debug`, `lto`, `debug_assertions`, `panic`, and `target-cpu` when the build set it). Those
  are baked in by a new `weft-bench/build.rs` from the environment cargo hands a build script, not read at
  run time, because by the time a manifest is written the active toolchain may be a different one and a
  released binary has no toolchain at all — a manifest that described the machine instead of the measured
  build would be evidence about the wrong thing. Alongside them: the git revision and whether the tree
  was modified, the hardware and engine fingerprint the report already carried, the allow-listed
  environment (one entry per variable, a value never read cannot leak), the command line and working
  directory, `--label name=value` operator labels, the `--claim <ID>` this run is evidence for, and a
  `grade` block naming every headline measure with its unit and the direction that is better
  (`latency.p50_ns` lower, `throughput.points_per_sec` higher, `storage.bytes_per_point` lower,
  `accuracy.rmse` lower, `correctness.passed` as 1 or 0) so a comparison has one vocabulary instead of a
  reader guessing which of two columns wins. Free text passes through a new `weft_bench::redaction`
  module — secret-shaped key names, known token prefixes, URL user-info and PEM blocks become
  `[redacted]`, `$HOME` becomes `~` — while an ordinary CPU model, file system or mount point is left
  exactly as it was, since a manifest that has eaten its own provenance is not evidence either. Capture
  refuses to write a manifest that does not verify: a credential still visible in it, a missing report, a
  digest that does not match, or a duplicated measure name. `weft-bench verify <MANIFEST>` re-checks a
  manifest later — required fields present, nothing credential-shaped left, the report found beside the
  manifest or at its recorded path, and its sha256 equal to the recorded one — prints every problem rather
  than the first, and exits non-zero; a manifest with no recorded revision, or built from a dirty tree, is
  reported but classed as advisory, so `--accept-advisories` can gate a development run on the record
  rather than on the state of the checkout.

- **A regression gate for benchmark numbers** (1.0 plan EVI-5). The harness could produce a report and a
  manifest, and nothing could answer the only question a nightly needs: is tonight slower than the last
  night we were happy with? Comparing two runs by eye is how a 12% latency walk to nowhere gets called
  noise, and how a real one gets called a bad machine. `weft-bench compare <BASELINE.json> <CANDIDATE.json>`
  pairs workloads by profile, adapter, workload and dataset shape — a changed point count or seed is a
  different workload, not a slower one — and rates each headline measure `flat`, `improved`, `suspicious`
  or `regression`, using the measure names and better-directions from the manifest's `grade` block so the
  two commands cannot disagree about which of two columns wins. A measure counts as a regression only when
  it worsens by more than `--threshold` (default 10%) **and** the two runs' bootstrap intervals do not
  overlap: a large move the noise still explains is reported as suspicious and does not fail the gate
  unless `--strict` says otherwise. A measure with no interval to begin with — throughput, storage size, a
  ratio or a count — can therefore never be a regression on its own, and `compare` says so in its note
  rather than inventing a confidence it does not have. The correctness flag is the exception: it has no
  magnitude to bound, so a workload that was correct and is not any more is a regression at any threshold.
  It refuses to compare across operating systems, architectures or report schema versions (a v22 column and
  a v23 column of the same name need not mean the same thing; `--ignore-provenance` overrides), tolerates
  the CPU, core count, memory and file system a
  hosted runner changes by itself, and treats comparing nothing at all as a failure, because a renamed
  workload would otherwise report success. It exits non-zero on a regression or a refusal; `--json` and
  `--markdown` write the same `Comparison`, so the verdict file and the exit code cannot drift apart. A
  new nightly workflow, `.github/workflows/bench-regression.yml`, builds the release profile, runs one
  fixed synthetic workload with a pinned seed, verifies its own manifest, and compares against the newest
  *successful* run of itself, so a red night stops being the baseline instead of becoming tomorrow's
  excuse; it is separate from `ci.yml` on purpose, because `scripts/release/verify.sh` reads CI as release
  evidence and a noisy night must not be able to veto a release.

### Changed

- **No silent narrowing in compute responses** (FRE-5): the downsample and interpolate
  endpoints converted each computed `BigDecimal` to the wire `f64` with a silent `0.0`
  fallback, so a result outside the `f64` range (for example a `sum` of two `1e308`
  samples) was returned as `0.0`. Such a value now fails the request with a `400`
  `not_finite` error. In-range values are unchanged.
- **Reduction tokens are exact-case** (ROADMAP, FRE-5). `Aggregation::from_token` now matches the
  lowercase wire tokens exactly, so `?agg=`, the JSON body's `aggregations`, the stored-range
  `agg=` and `weft-bench --ds-aggs` refuse `Min`, `SUM` and `MEDIAN` with a `400` (or a CLI error)
  that names the token. **This breaks v0.1.0**, whose query string folded case: send `min`,
  `sum`, `p50`, `twa_linear` and the other lowercase spellings the README lists. The
  underscore spellings, the aliases and the bare lowercase names are unchanged.

- **The segment-set rollup lives in `segment_index.db`** (1.0 plan D-S11). The
  rollup row (`aspect_metadata`) was written to `metadata.db` in a second transaction
  after a seal's index commit, and rebuilt in a second transaction after a swap's —
  so a crash between the two left the rollup behind the index it summarizes, and a
  failed rebuild left it stale with only a warning. Now the seal's transaction folds
  its segment into the rollup row, and the swap's transaction writes the rollup its
  post-swap rows derive, in the same `segment_index.db` transaction that commits the
  rows. `SegmentStore::aspect_metadata`, `store_stats` and the fragmentation guard
  read the rollup from `segment_index.db`; the journal replay still re-derives it as
  a no-op reconciliation. `metadata.db` keeps the rows-mode legacy catalog, and its
  rollup table is the pre-S11 home, no longer written by the store.
- **The README stops promising what no clone can deliver.** No benchmark artifact — manifest or
  result JSON — is committed anywhere in the tree, so its Performance section now distinguishes a
  figure that names the bench it came from from one that is only a dated local measurement, and
  points the remaining gap at the claims registry (roadmap EVI-2). Documentation only; no number
  was changed or dropped.
- **`tokio` is 1.53.3, not 1.53.2.** RustSec advisory RUSTSEC-2026-0386 (`unsound`) reports that
  `AsyncFd`'s safe constructors — `new`, `with_interest`, `try_new`, `try_with_interest` — accept
  any `T: AsRawFd` rather than proof that the caller owns the descriptor, and that a descriptor
  duplicated and then closed while an `AsyncFd` is live can leave a freed registration behind in
  `epoll`'s user data, which is a use-after-free when a readiness event fires. Tokio deprecates
  those constructors in favour of `AsyncFd::register` from 1.51.6 and 1.53.3, so the fix here is a
  patch release: the lockfile moves one version and one checksum, and no API this workspace calls
  changed name or signature. Nothing in this repository constructs an `AsyncFd` — `grep -rn AsyncFd
  --include='*.rs' .` finds no match — but `weftdb` and `weft-server` both link tokio directly and
  everything else reaches it through turso, axum, hyper-util, tonic and reqwest, so every release
  archive built before this commit carried the flagged version. `deny.toml` keeps
  `[advisories] ignore = []`: an advisory that names a released fix is upgraded, not muted.

- **Every crate declares its own version, and the workspace declares the edition** (1.0 plan FRE-1).
  `[workspace.package]` carries no `version` any more and each of the ten members states
  `version = "0.1.0"` where it inherited one, because a release is a statement about specific crates
  and a patch of one library is not a reason to put a number on nine that did not change. The release
  pipeline is unchanged: `weft-server`'s version is still the one a tag names, which is what
  `scripts/release/verify.sh` reads and refuses a disagreeing tag about. `edition` went the other way —
  the workspace states `edition = "2021"` and every member inherits it, so FRE-22's move to edition
  2024 is one edit in one file instead of ten in ten. The internal `[workspace.dependencies]` path
  entries keep their explicit `version = "0.1.0"` requirements, and `scripts/crates/check.py check`
  (see Added) is what keeps those requirements and the members' versions in step. Each published crate
  now states `documentation = "https://docs.rs/<crate>"`, and `weftdb` carries a `README.md` of its
  own: it inherited the repository's, so its crates.io page would have described a server and a
  terminal UI on a library's page.
- **Library crates stop deciding their users' allocator and GPU stack** (1.0 plan PAC-3). No
  library in this workspace enables turso's `mimalloc` feature or splimes' default `gpu` feature
  any more, because both are decisions a dependency makes for the whole process. turso 0.8.1
  installs a `#[global_allocator]` in its own crate root when `mimalloc` is on, so every program
  linking `weftdb` — an embedder with its own allocator, a program that wants the system one — was
  switched to mimalloc without being asked; `gpu` drags `wgpu`, and with it the Vulkan, Metal and
  DX12 backends, into the graph of everything that depends on splimes. Counting the distinct
  packages in each crate's non-dev dependency graph, before and after: `weft-line-protocol` and
  `weft-reduce` 77 → 25, `weftdb` 266 → 227, `weft-orchestration` 267 → 231. Our own GPU users —
  `weft-server` and `weft-bench`, which call `prewarm_gpu`, `calibrate` and `gpu_pool_stats` —
  enable `gpu` themselves, as do `weftdb`'s interpolation benches through a dev-dependency, so
  nothing this repository runs lost its GPU path. A library user who wants either asks for it: an
  allocator by declaring one in their own binary, the GPU by enabling `splimes/gpu`. ⚠️ Rust code
  that reached the GPU *through* `weftdb` — a `splimes::Backend::Gpu` while depending on `weftdb`
  alone — names `splimes` itself now. One default stays on deliberately: splimes' `serde`, because
  `weftdb::BatchedMeasurement` derives `Serialize`/`Deserialize` over a `splimes::Resolution` field.
  turso's other default, `fts`, stays off, as it already was: it is turso's full-text search, and
  turning it on grows the graph with tantivy, tantivy-columnar, zstd-sys and the rest of that tree,
  which the first attempt at this dependency line showed in the lock diff.
  One number to be aware of: `cargo bench -p weftdb` is criterion, not test code, so it used to
  inherit mimalloc through turso and now runs on the system allocator — a `weftdb` bench figure
  from before this release is not comparable with one taken after. `weft-bench` never linked
  `weftdb` and never had mimalloc, so the Weft-Bench series, including the nightly regression
  track, does not move.
- **`tracing-subscriber` is a dev-dependency of `weftdb`, not a dependency.** No code in the
  library installs a subscriber outside `#[cfg(test)]`, so every program linking `weftdb` was
  building registry, fmt and env-filter code it never called.

- **Statuses split where the §4 table names a different condition** (1.0 plan X-ERR). A batch
  that will not seal under an aspect's declared schema no longer answers `400` whatever the
  reason: a length mismatch between the two columns, a zero `rows_per_page`, and a backwards
  timestamp stay `400` (the batch itself is malformed), while a value the declared encoding
  cannot hold — an overflow, or a loss beyond the declared `value_tolerance` — is `422
  value_not_representable` with `details.max_abs_error` and `details.tolerance`, the same call
  `/api/v1/interpolate` already made for a value outside the physical type's range; a value with
  no finite image is `400 not_finite`, with the row in `details.row`. A name the store cannot
  address as a frame file is `400 invalid_aspect_name` carrying `details.aspect` and
  `details.reason`, at declare as well as on use. A store root this binary refuses to open is
  `503 incompatible_store`, the HTTP shape of the CLI's exit `65`. `POST /api/v1/downsample`
  answers `422 value_not_representable` for a timestamp that cannot be bucketed at the requested
  resolution, and `400 not_finite` for a value with no finite `f64` image, where both used to be
  reported as `500`.
- **Time-weighted averages run on integers** (`weft-reduce`): `twa`, `twa_linear` and
  `twa_bucket_end` over samples with `i64` mantissas at scales up to 17 compute the weighted sum
  in `i128` and divide once with the integer long division `avg` uses, reproducing the `BigDecimal`
  result in value and representation (including `bigdecimal`'s special cases for a value of one
  and for zeros); other samples take the `BigDecimal` loop as before. Hourly `avg,p99,twa` over 1M
  real BTC closes: 150 → 55–58 ms on points, 119 → 38 ms on stored mantissas; per stored segment
  (decode included) 155 → 79 ms.
- **Integer-path reductions spend less per bucket** (`weft-reduce`): `reduce_scaled` and
  `reduce_partial_scaled` close time-ordered buckets into a `Vec` instead of a map insert per
  bucket (out-of-order input still merges through the map), `reduce_scaled` builds a bucket's
  `sum` decimal only when `sum` is requested, and `reduce_scaled` and `PartialReduction::finish`
  size their output once. `reduce` and `reduce_partial` on `BigDecimal` points do the same, and
  skip rescaling when every value already has the series' scale. Results are unchanged. Hourly
  `avg` over 1M real BTC closes went from ~3.1× an `f64` loop to ~2.1–2.2× on stored mantissas
  (4.26–4.61 ms against 2.06–2.07 ms in the same runs), and from 24.1 to 21.0 ms (~10×) on points.
- **Exact percentiles (`p50`/`p90`/`p95`/`p99`) no longer clone and sort every bucket**
  (`weft-reduce`). A single percentile selects its rank in linear time over borrowed values
  and several share one sort; only the result is cloned. Results are identical, including
  which of two numerically equal values (`1.0`, `1.00`) is returned. A lone hourly `p99`
  runs ~1.6–3.4× faster on the bench (two-decimal, real BTC and full-expansion series),
  which made it about twice as fast as `sketch_p99` until the next entry.
- **`SegmentStore::interpolate_range`** (`weftdb`): interpolate-on-read over stored segments
  — the aspect's samples in a window (plus a `margin` of knots past each edge) run through a
  caller-supplied splimes `Interpolator`, returning the grid with provenance;
  `interpolate_range_f64` returns `f64` values (~3× faster at 1M points, since building a
  `BigDecimal` per output point dominates). A read window holding more than 4 Mi stored rows
  (`interpolate_range::MAX_KNOTS`) is refused. Library only; no HTTP endpoint yet.
- **`weft_reduce::decimal_to_f64`**: a `BigDecimal` to `f64` conversion bit-identical to
  `bigdecimal`'s `to_f64`, fast for values of at most 15–16 significant digits.
- **Stored-range percentile and time-weighted-average downsamples take the integer path**
  (`weft-reduce`): `reduce_partial_scaled` serves `p*` and `twa*` too, collecting samples
  from the stored mantissas, so `GET …/storage/{aspect}/downsample?agg=p99,twa` on a
  `ScaledI64` aspect no longer decodes each segment twice. Results are unchanged; ~1.4× per
  segment on the bench.
- **`reduce` and `reduce_partial` compute on integers when the values allow it**
  (`weft-reduce`): when every value fits an `i64` mantissa at the series' largest scale, the
  reduction compares and sums integers and returns the same buckets, value for value and in
  representation. ~2.3× on real price series, ~4× on two-decimal ones.
- **Bucket averages that never terminate are computed in one step** (`weft-reduce`): their
  100 significant digits come from one big-integer multiplication and one division by the
  count instead of a digit loop, with an identical result. Hourly `avg` on the integer
  path runs ~1.7× faster (median 15.3 → 9.0 ms per 1M rows), and every `avg` gains from it.
- **`sketch_p*` reductions skip the logarithm for a value in the previous value's bucket**
  (`weft-reduce`), with identical sketches; ~1.3–1.4× on real price series.
- **`sketch_p*` reductions convert most values to `f64` by one division** (`weft-reduce`).
  A value `m × 10^-s` with `|m| < 2^53` and `0 <= s <= 22` is converted as
  `m as f64 / 10^s`, which is bit-identical to `bigdecimal`'s `to_f64`, so sketches and
  results do not change; `ScaledI64` segments feed their mantissas straight in. Hourly
  `sketch_p99` runs ~2× faster on two-decimal and real BTC series, and a stored segment's
  `avg,sketch_p99` downsample ~2× faster on the integer path. Values outside those bounds
  take the previous conversion.
- **Breaking for Rust users: `weftdb` reads no `WEFT_*` variable when it opens a
  segment store, and never exits the process** (release plan C-1).
  `SegmentStore::open` and `open_scoped` now use `SegmentStoreOptions::default()`
  (no checkpoint index, no partial sidecars, no bit-sliced codec) whatever the
  environment says; pass `SegmentStoreOptions::from_env()` to the new
  `SegmentStore::open_with_options` / `open_scoped_with_options` to keep reading
  `WEFT_SEGMENT_*`, as `weft-server` does. `WEFT_ON_AMBIGUOUS_COMMIT` is read by
  `weft-server` alone; an ambiguous COMMIT only poisons the store, and the server exits
  with status 70 on that under `exit`. Deployments of `weft-server` see no difference.
- **Stored frame paths are resolved by splitting on both `/` and `\`**, so a store
  written on Windows reads on Unix and the other way round, and a path that would lead
  out of `segments/` (a `..`, no `segments` component) is refused instead of read
  where it points. Upgrading a layout-1 store whose index records such a path fails
  with `StoreError::UnsafeLegacyPath`, naming the aspects, before any migration applies
  or a marker is written; nothing is moved or quarantined.
- **Breaking for Rust users: `SegmentIndexStore`, `AspectMetadataStore`, `AspectCatalog`
  and `CatalogStore` can no longer be opened on their own.** Their `open` and
  `open_in_memory` are gone: they created this build's schema in any file, without the
  store's `STORE_FORMAT` gate, which would let a WeftDB write tables into a store a newer
  one wrote. A `SegmentStore` opens all four (`index()`, `metadata()`, `catalog()`,
  `registry()`); nothing in the workspace opened them otherwise.
- A store root's parent that cannot be fsynced because its filesystem refuses (`EROFS`,
  `EINVAL`, `ENOTSUP`: read-only mounts, FUSE and Docker Desktop bind mounts) is now
  skipped with a warning when the root predates the open, as an unreadable parent
  already was.
- Adding a missing column during a migration now ignores only Turso's exact
  duplicate-column error for that column; any other failure fails the open (the
  `metadata.db` order-health column used to swallow every error).
- **A segment store root can be open in only one `SegmentStore` at a time.** ⚠️ A
  second `SegmentStore::open`/`open_scoped` on a root that this process already has
  open used to succeed, so several scopes could share one root at once. It now fails
  with the new `StoreLocked` error (downcast it from the `anyhow::Error`). Embedders
  that kept several scoped stores open on one root must close one before opening the
  next, or give each scope its own root.
- **Control-plane backups are published atomically, with a manifest.** A backup is
  built in a `.partial-{label}-{nonce}/` directory beside its destination, each
  database verified and fsynced, then a `MANIFEST.json` (format, creation time, and
  each file's size, tables and rows) is written and fsynced, the directory fsynced,
  renamed to its label and the parent fsynced. A backup directory now holds the four
  databases plus `MANIFEST.json`. Backing up into a directory that already exists is
  refused (it was accepted when empty). A backup that fails removes its build
  directory again, unless it fails after the rename (only the parent's fsync is left
  then): that error, a `500` over HTTP, leaves the complete backup under its label.
- **Backup retention counts only complete backups**: a `backup-<digits>` directory
  with a `MANIFEST.json`, or one from before manifests that holds all four databases.
  Pruning renames a backup to `.deleting-*` (durably) before removing its files. A
  backup directory that cannot be inspected (a permission error, say) is skipped and
  logged instead of failing retention. The backup daemon removes `.partial-*`,
  `.deleting-*` and `.restore-drill-*` entries left untouched for an hour, at start
  and on every tick; without the daemon, what a crash leaves there stays until
  removed by hand.
- **A backup label that is already taken is a `409` with `"code": "already_exists"`**
  (it was a `400`); the existing backup is untouched. `weft_server::StorageError` gains
  the `AlreadyExists` variant for it, and the JSON error body carries a `code` field
  for that error only.
- **`restore_control_plane` stages each file as `<name>.tmp`**, synced and verified,
  renames them into place only once all four verify, and fsyncs the root, which it
  now creates durably. A backup with a manifest must match it (sizes, tables, rows).
- Library API: `verify_snapshot` and `snapshot_with_verify` take the tables the
  snapshot must hold, and `SnapshotReport` lists them in `table_names`.
  `sweep_backup_staging_at` is `sweep_backup_staging` with an explicit clock. `StoreFs`
  gains `create_dir`, `remove_dir_all` and `copy_new`, and the `SimFs` power-cut
  simulator (`fault-injection` feature) now models directories.
- **Breaking for implementors of `Inputs` and `Outputs`.** Three required methods were
  added without default implementations: `Inputs::dequeue_unbatched_entries`,
  `Inputs::remove_extracted_batches` and `Outputs::get_unbatched_entries` (see Added).
  Behaviour that implementations must now match: `insert_unprocessed_batch` and
  `batch_insert_unprocessed_batches` return `Ok` when they skip a batch that is already
  queued, processed or extracted; a processed batch's `batch_hash` is the hash it was
  queued under, not a hash of its processed measurements; and
  `enqueue_unbatched_measurements` is an upsert that moves an already-queued entry's
  `queued_at` forward and reports a write-write conflict as
  `Error::TransientMvccError`.
- **Rows-mode ingest and the incremental build commit more often** (crash-consistency
  fixes below). `capture_measurement` makes four synced commits where it made three
  (the write-ahead enqueue, the row, the second enqueue, the transaction-log entry), and
  `batch_capture_measurements` one more per chunk (each chunk's second enqueue). The
  incremental build dequeues in transactions of at most 5,000 entries, one synced
  commit each, where a run dequeued in a single commit, so a backfill of a million rows
  costs about 200 dequeue commits.
- `weft-tui` lists databases with `Database::list_stored_databases`: the names are
  sorted, and an unreadable data directory is reported as an error instead of showing
  an empty list (a missing one still lists nothing).
- **A segment store's `segment_index.db` is migrated to store layout 2 when it opens**
  (crash-consistency design, S6). The migration is additive and idempotent and
  rewrites no data: `segment_index` gains the columns `gen` (0 on every existing row),
  `prec`, `frame_crc` and `commit_epoch`, and the database gains the tables
  `aspect_seq`, `frame_journal`, `ingest_ledger`, `segment_quarantine`, `store_meta`,
  `aspect_metadata` and `segment_changes`, which later releases fill. `store_meta`
  records `layout_version = 2` and a `store_uuid`. ⚠️ The migration is one-way:
  v0.1.0 would still open a migrated store, ignoring the additions, but do not run it
  on one again (see the upgrade note at the top of this section). A WeftDB refuses a
  store whose `layout_version` is newer than it knows, before it writes anything to its
  databases (the open has already taken the root's `LOCK`, and created `segments/` if
  it was missing).
- Segment-index writes (seals, reconciles, splits, merges, squashes) commit through
  one transaction type. A write that loses an MVCC conflict to another writer of the
  same row still fails the call at once, as before: retrying it would replay it over
  the other writer's committed row. Only a database that is busy before the
  transaction starts is retried, up to five times with backoff.
- **Segment ids come from a persistent per-aspect allocator** (crash-consistency design,
  S7). A seal took `MAX(id) + 1` for its aspect; it now takes the next id of an
  allocator that hands each id out once, writes its frame, and commits its row with a
  plain `INSERT` (never `INSERT OR REPLACE`) together with the allocator's raise, which
  is persisted in `segment_index.db`'s `aspect_seq` table. A deleted segment's id, even
  the highest, is never reused, before or after a restart, and neither is the id of a
  frame a seal wrote but crashed before committing: the allocator starts above every
  `{aspect}-{id}.weftseg` (and `.weftpart`) name in `segments/`. Ids still grow, but no
  longer as `MAX(id) + 1`: after a squash or merge deleted the highest segments, the
  next seal's id leaves a gap. The suffixes that splits and split overlap merges create
  take their ids from the same allocator. Each aspect's control-plane writes (a seal's
  row and rollup fold, maintenance row rewrites and deletes, rollup rebuilds) take turns
  under a per-aspect lock, so they no longer conflict with one another, and a seal's
  index transaction is retried (up to five times) if anything else does conflict with it.
- **Maintenance operations on one aspect take turns.** A reconcile, split, overlap
  merge, squash or compaction now holds its aspect for its whole run. The HTTP
  maintenance endpoints (`POST /api/v1/storage/{aspect}/reconcile`, `…/squash`,
  `…/compact` and the store-wide `POST /api/v1/storage/reconcile`) wait up to 30 s for
  an aspect another operation holds and then answer **`409 Conflict`**, naming it; the
  store-wide sweep maintains the other aspects first. The background reconcile daemon
  never waits: it skips a busy aspect until its next tick (logged at `DEBUG`, counted
  in the tick span's `busy` field, not as a failed pass). ⚠️ Library API: the store-wide
  sweeps (`reconcile_all_over_threshold`, `reconcile_all_hot_cold`,
  `reconcile_all_overlaps`, `reconcile_all_overlaps_with_policy`,
  `squash_all_over_threshold`, `squash_all_to_target_rows`,
  `squash_all_to_target_rows_if_fragmented`) take a new last argument,
  `MaintenanceWait::Skip` or `MaintenanceWait::Wait(duration)`, and their results gain
  a `busy` list of the aspects they could not take. The per-aspect entry points keep
  their signatures, wait up to `SegmentStore::maintenance_wait()` (30 s,
  `DEFAULT_MAINTENANCE_WAIT`; change it with `with_maintenance_wait`), and then fail
  with the new `MaintenanceBusy` error.
- **Reconcile and split are write-once** (crash-consistency design §5.3-5.4, S8).
  `reconcile_segment` (and every reconcile pass built on it) and `split_segment` no
  longer rewrite a `.weftseg` frame. Their outputs are new frames in `segments/` named
  `{enc(aspect)}~g{gen}~p{prec}.weftseg` (the aspect name encoded as by
  `aspect_name::encode`, a per-aspect generation that is never handed out twice, and the
  adoption order; an aspect name that would encode past the file-name limit is cut and
  completed with a hash, `…~h{16 hex digits}~g…`), journaled as pending in
  `segment_index.db`'s `frame_journal`, written and fsynced, then `segments/` fsynced.
  One `segment_index.db` transaction then swaps them in: it replaces the segment's row
  only if it is still the version that was read (id, generation and frame CRC; another
  version fails the operation with a `Conflict` and writes nothing back), inserts a
  split's suffix under an id allocated inside it, journals the old frame as retired,
  and advances the aspect's epoch and generation counter. The old frame is unlinked by
  the reaper once no read that may still open it is running (every read pins the
  reclaim epoch it started in, released when its future is dropped, timed out or not),
  then its journal row is deleted; a frame the filesystem refuses to unlink for now (a
  Windows sharing violation, `EBUSY`) waits for a later pass instead of failing anything.
  Every open replays the journal before it returns: outputs a crash left before their
  swap are unlinked, retired frames no row references are unlinked, `segments/` is
  fsynced, the rows are deleted, and the rollup of an aspect whose swap committed is
  rebuilt. A reconcile also checks that the frame it reads is the one its row was
  committed with (its length, and its CRC once bound), and the sidecars of its outputs
  are written under a `.tmp-*` name and renamed into place. The overlap merge, squash
  and compaction became write-once too (see the next entry). An older layout-2 WeftDB
  reads the new frames (the index keeps recording root-joined paths) but does not
  replay the journal.
- **The overlap merge, squash and compaction are write-once** (crash-consistency design
  §5.3 and §7, S9). `reconcile_overlaps`/`reconcile_overlaps_with_policy` (both the full
  rewrite of a component and the split of its cold prefix), `squash_aspect` and
  `squash_aspect_to_target_rows`, and every sweep and threshold variant built on them, no
  longer rewrite a segment's frame in place and then delete the other members in commits
  of their own. Each component, squash or compaction group is now one swap, as a
  reconcile's is: its output frames are written under new names, fsynced, and
  `segments/` fsynced, then one `segment_index.db` transaction replaces the members'
  rows with the outputs' and deletes the members left over, each only if it is still the
  version that was read (a member another writer changed fails the swap with a
  `Conflict`, writing nothing), and journals every member's frame as retired for the
  reaper. The outputs' rows record the frame actually written, so a paged member merged
  into a single-block output never leaves a row describing the wrong frame kind, and
  carry the largest adoption order (`prec`) of their members. Merges also check that each
  member's frame is the one its row was committed with, and merge on a blocking thread.
  Outputs take their members' ids, the `j`-th output in time order the `j`-th smallest
  (release plan D17, without a change to the `segment_index` key), and are cut only at
  timestamp boundaries, so a run of equal timestamps never spans two outputs.
- ⚠️ **An overlap merge that splits its component no longer allocates an id for the
  suffix.** The hot suffix takes the component's second-smallest segment id (the cold
  prefix keeps the smallest), where it used to take a fresh id from the allocator. The
  segment ids an overlap split leaves, and those later seals receive, change
  accordingly; the merged data does not.
- ⚠️ **`split_segment` refuses a split whose suffix a newer segment overlaps.** The
  suffix takes an id above every segment of the aspect, so where it shares a timestamp
  with a segment sealed after the split one it would outrank that segment's newer
  value. The split now fails with an error, changing nothing, when any segment with a
  higher id overlaps the suffix's span (its first timestamp at or after `boundary` to
  the segment's last); the check runs again under the aspect's commit lock right before
  the swap, against a seal that committed meanwhile.
- **The stored-range downsample reduces `ScaledI64` segments on their mantissas.**
  `SegmentStore::downsample_range` (behind `/api/v1/storage/{aspect}/downsample` and its
  CSV, Arrow and Parquet forms) reads a segment's window in its physical encoding and uses
  `reduce_partial_scaled` when every present value in it is a `ScaledI64` at one scale and
  every requested reduction is a streaming one or a `sketch_p*`; anything else takes the
  `BigDecimal` path as before. Results are unchanged.
- **Every reduction computes `avg` by integer long division**, in `weft-reduce`'s `reduce`
  and partials as well, whenever the bucket sum's unscaled integer fits an `i128`. The
  quotient equals `bigdecimal`'s own division in value and representation (scale
  included); a larger sum still divides as a `BigDecimal`.
- **Faster reads, same bytes on disk** (`weft-physical-type`). The frame checksum is
  computed sixteen bytes per step (slicing-by-16, the same CRC-32, so every stored frame
  still verifies); the bit-packed value and timestamp codecs, and the f64 codecs behind
  `experimental-codecs`, read a whole field per load instead of a bit at a time; the
  batch point read (`read_segment_points`, `/api/v1/storage/{aspect}/at-multi`) walks each
  value block once instead of once per instant; and the bit-sliced tile decoder
  (`bitsliced-codec`) is branch-free. The bench schema is now v19.

### Deprecated

- `SegmentIndexStore::next_id` (`MAX(id) + 1`): it reissues the id of a deleted or
  crashed segment and races concurrent callers. `SegmentStore` no longer uses it; it is
  kept for tests.

### Fixed

- **Range reads no longer slow down as segments pile up, and no longer fail during a seal**:
  the segment index's time prune names its time index and seeks `min_ts` between
  `start − widest span` and `end` (the widest span per aspect is loaded once and widened by every
  index write before it commits), where the planner had walked every segment of the aspect; and
  its read connections wait up to ten minutes on a lock instead of failing `database is locked`.
  Write transactions still see `Busy` at once, as their retry protocol requires.
- **`analyze_range` stays on its step grid past 50,000 points**: it cut a long range into chunks
  that were not a whole number of steps, so every chunk after the first was resampled between the
  steps it should have landed on (20 s off the minute at 100k UCI rows), and the peak detector
  reported a peak 400 s from the real one. Chunks now span whole steps, and the point two chunks
  share is returned once.
- **Opening a legacy `Database` no longer walks `/proc`**: its cache sized itself with
  `sysinfo::System::new_all()`, which reads every process and thread on the machine; it now reads
  memory only. On a busy machine that was most of each `Database::existing`, which
  `analyze_range` calls per chunk — event detection on 3k rows went from 0.9 s to 21 ms.
- **Cross-engine load timing no longer includes the previous load's teardown**: `weft-xbench`
  now releases each rep's database or store before starting the next load's clock. Load rows
  measured before this (the EVI-15 report among them) counted the drop of the previous rep.
- **The JSON body's `aggregations` accepts the documented reduction spellings.** `POST /api/v1/downsample`
  read its tokens through the bare lowercase variant names only, so the README's `twa_linear`, `sketch_p99` and aliases such as
  `median` were rejected as unknown variants, and a name copied from a response (`twa_linear`) could not be sent back. The body
  now reads the query string's parser (`Aggregation::from_token`), so both surfaces accept the same set; the bare names
  (`twalinear`, `sketchp99`) still parse. An unknown token is a `400` that names it and lists the accepted spellings (ROADMAP, FRE-5).

- **A pre-S11 store read zero metadata until its next seal or swap (D-S11).**
  Moving the rollup to `segment_index.db` left stores written before S11 with no
  rollup rows at all, and the journal replay only re-derives the aspects its
  journal names — so a clean pre-S11 store opened with `aspect_metadata`
  reading default zeros (the v0.1.0 compat gate caught it: a migrated aspect
  read 0 nulls against the 14 v0.1.0 counted). Open now backfills exactly the
  missing rows after the journal replay; S11-native stores, whose rows exist,
  pay one `SELECT` per aspect and nothing more.

- **Power loss during or after a reconcile or split could lose or tear rows.** Both
  rewrote the segment's frame in place with an unsynced write (a split also wrote its
  suffix that way), so a power cut, even after the call returned, could leave a
  committed row over an empty, torn or zero-filled frame: its rows gone and every read
  over them failing with a checksum error. Their outputs are now fsynced, under new
  names, before the commit that switches to them (see Changed).
- **A read during a reconcile or split could fail or see rows twice.** A read could open
  a frame half way through its in-place rewrite, and a split committed its suffix before
  it shrank its prefix, so a read in between returned the suffix's rows twice. A swap is
  now one transaction, and the frames it retires stay until no read that may open them
  is running.
- **Power loss during or after an overlap merge, squash or compaction could lose or tear
  rows.** Each rewrote its lowest segment's frame in place with an unsynced write, then
  deleted the other members' rows and frames one by one, so a power cut, even after the
  call returned, could leave the merged row over an empty, torn or zero-filled frame,
  with the members whose rows it had absorbed already deleted: their rows gone and reads
  over the merged range failing with a checksum error. A crash between the rewrite and
  the row update could also leave a paged segment's row over a single-block frame. Each
  merge is now one swap of fsynced new frames (see Changed).
- **A read during an overlap merge, squash or compaction could fail or see rows twice.**
  Between a merge's rewrite of its lowest segment and the deletion of the others, a read
  returned the merged rows beside the members' own, and a read during the rewrite could
  open a half-written frame. The swap is now one transaction, and the frames it retires
  stay until no read that may open them is running.
- **An overlap merge's split suffix could outrank a newer seal.** The suffix took a fresh
  id from the allocator when the merge wrote it, above that of a seal that had taken its
  id meanwhile, or before the merge but committed after it, so where the two shared a
  timestamp the merge's older value won over the seal's acknowledged one. The suffix now
  takes a member's id, below the seal's.
- **A reconcile whose segment another operation changed meanwhile wrote it back.** Its
  swap now requires the segment's row to be the version it read and fails with a
  `Conflict` otherwise (the maintenance lock already keeps the store's own operations
  apart; this holds for anything else that writes the index).
- **Concurrent seals of one aspect could lose an acknowledged batch.** Two seals that
  ran at once took the same id: the later one's write replaced the earlier one's frame
  and its row, so a batch whose seal had succeeded was gone, or one of the seals failed
  with an MVCC write-write conflict. Every seal now gets an id of its own (see the
  allocator entry under Changed).
- **A seal could reuse the id of a deleted segment or overwrite a crashed seal's
  frame.** After a squash or merge deleted an aspect's highest segment, the next seal
  took its id again, so a seal racing the merge's unlink could lose its frame, and a
  leftover sidecar of the deleted segment could be served for it. A seal that crashed
  after writing its frame and before committing it left the frame behind, and the next
  seal truncated it.
- **A reconcile racing a squash or merge of the same aspect could bring a merged
  segment back.** A reconcile that had read a segment wrote it back after the merge
  had folded it into its target and deleted it, so its stale values outranked the
  merged ones and its rows read twice. Maintenance operations on one aspect now take
  turns (see Changed).
- **Concurrent seals could leave the aspect's rollup (`metadata.db`) wrong**, missing
  some of them, or fail after their row had committed: each folded itself in with an
  unlocked read-then-write. The fold, and rollup rebuilds, now run under the aspect's
  lock with the seal's commit.
- **A segment store that was moved, restored into another root or mounted at another
  path could not read its frames.** The index records each frame by the absolute path
  it was written under, and every read opened that path. Reads now resolve it against
  the store's current root: a path under the root is used as it is, and any other is
  taken as `root/segments/` plus what follows its last `segments` component. Restoring
  a control-plane backup next to copied frames no longer needs the original root to
  still exist.
- **A segment store root is now owned by one process.** Opening a store takes a `LOCK`
  file in the root (with the holder's pid and session in `LOCK.holder` beside it) and
  holds it until the store closes. A second `weft-server` on the same
  `WEFT_SEGMENT_STORE_ROOT` now exits at startup with an error naming the pid that
  holds the root, instead of Turso's "File is locked by another process". The lock is
  released by the OS however the holder exits, so there is never a stale lock to
  clear. (In-process, see the `StoreLocked` entry under Changed.)
- **A control-plane database that could not switch to MVCC was opened anyway.** The
  segment store's four control-plane databases now refuse to open unless they run in
  MVCC journal mode and a new connection syncs FULL, which is what makes a COMMIT
  durable when it returns. Previously a failed switch was ignored and the database
  committed in WAL mode.
- **Opening a control-plane database now syncs the MVCC header the switch to MVCC
  wrote.** The switch writes the MVCC header into the database file without syncing
  it, and commits then sync only the `-log`, so a power cut could leave a header that
  still says WAL beside a log of acknowledged commits, which Turso refuses to open.
  Turso synced it anyway for a database that never ran MVCC, but not for one switched
  back to WAL. Every open of each of the four databases now commits a no-op
  transaction and runs a TRUNCATE checkpoint, which makes Turso fsync the file, header
  included, before the open returns. Every open, not only the one that switched: an
  open that crashed between its switch and that sync leaves a header the next open
  reads back as MVCC although it never reached the disk. This costs one commit and one
  checkpoint per database per open.
- **Opening a store now makes its files' directory entries durable.** The root,
  `segments/`, the root's parent and any directory the open created are fsynced, so
  the control-plane databases and their logs cannot vanish from the directory after a
  power cut. A pre-existing root whose parent the server cannot read still opens, with
  a warning.
- **The store's database/subject registration is one transaction.** A crash during
  open could leave the database registered without its subject.
- **A backup that stopped part-way could count as a backup.** A crash or error
  mid-backup left a `backup-<digits>` directory with only some of its databases,
  which retention counted (so it could prune a good backup in its place) and a drill
  could pick. A pruned backup interrupted part-way was left half-removed under its
  name. Neither can happen now; see above.
- **A reported backup could be lost on power loss**: nothing fsynced its files'
  directory entries or the directory itself.
- **An empty control-plane snapshot passed verification.** Every snapshot must now
  hold its database's tables, at backup and at restore.
- **The restore drill discarded a failed cleanup.** `POST /api/v1/storage/restore/drill`
  now reports it in a new `cleanup_error` field (`null` when the rehearsal copy was
  removed).
- **Two restore drills started in the same millisecond shared a rehearsal directory**,
  so whichever ended first removed the directory the other was still using. A drill
  now claims its `.restore-drill-<millis>` directory before restoring into it, and one
  that finds it taken answers `409` with `"code": "already_exists"`, leaving it alone;
  retry it.
- **`Database::new` could leave a half-created database** that neither a retry of
  `new` ("already exists") nor `Database::existing` (no `database` row) could use. A
  database is now built in a hidden `.{name}.creating-{nonce}` folder beside its final
  place and renamed to `{name}` only once its schema and `database` row are committed,
  then the data directory is fsynced. An error or crash leaves either nothing under
  `{name}` (retry `new`) or the complete database (open it with `existing`). Build
  folders left by a crash are removed by the next `new` or listing; the data directory
  gains a `.weft-creating.lock` file that keeps a sweep away from a creation in
  progress, in this or another process. `new` now refuses a name of the build folders'
  form (`.{x}.creating-` followed by 32 hex digits). The rename is the commit point:
  if the data directory cannot be fsynced after it, or the published database cannot
  be opened at its final path, the database stays in place (another task or process may
  already have opened it) and the error says it was created and to open it with
  `existing`; after a failed fsync it also says the creation may not survive a power
  loss. The checkpoint after creation is best-effort.
- **Legacy ingest could store rows that were never batched.** `batch_capture_measurements`
  and `capture_measurement` queued the timestamps for batching only after the rows
  committed, so a failure in between left rows the incremental pipeline never saw.
  The timestamps are now queued before the insert, and queued again once their rows (or
  each chunk) are committed. The incremental build no longer fails on an aspect whose
  queue holds timestamps but no rows yet.
- **An incremental build running during an ingest could drop the ingest's rows.** It
  could read a queued timestamp before its row was committed, build the window without
  it and dequeue it, and it cleared the whole queue when every queued timestamp lay
  before the earliest stored row, as a backfill's do. Enqueuing a timestamp that is
  already queued now moves its `queued_at` forward (it used to keep the old one), and
  the build dequeues only the entries it read with the `queued_at` it read, so the
  second enqueue after the commit keeps the row queued for the next build. The build
  now reads the earliest measurement, which it aligns its windows on, from the rows on
  every run: a cached value (kept per `Database` instance for up to 10 minutes) made it
  clear a backfill from the queue. Ingest also drops that cached value after each
  commit, but only in the instance (and its clones) that ingests; other instances, such
  as the one each weft-tui action opens, keep theirs until it expires. A window the
  build batched from rows that were already committed is rebuilt identically after the
  second enqueue, and the batch dedupe skips it. Residuals, for a build running while
  ingesting into the same aspect: the ingest's rows stay unbatched if the ingest dies
  (or exhausts its retries) between a commit and its second enqueue; and a build run
  between two chunks of a backfill or gap fill larger than one chunk batches the
  windows spanning committed rows and rows still to come with the latter interpolated,
  so those windows get a second, different batch and occurrence once the rows land.
- **Concurrent writes to the batching queue failed the call.** The queue enqueue is an
  upsert, so it writes a timestamp that is already queued, and an MVCC write-write
  conflict with a consumer's dequeue, or with another ingest of the same timestamps,
  failed the ingest or the consumer's dequeue. Every ingest path's enqueue
  (`capture_measurement`, `batch_capture_measurements`, `capture_new_measurement`,
  `capture_measurement_chunk`, `capture_new_measurement_chunk`) and the consumer's
  dequeue now retry such a conflict, for about three seconds; a single
  `enqueue_unbatched_measurements` call returns it as `Error::TransientMvccError`. The
  consumer dequeues in transactions of at most 5,000 entries, so an enqueue waits for
  one of them, not the whole dequeue. `batch_capture_measurements` queues all of its
  timestamps in one transaction before it stores any row, so a consumer dequeuing some
  of the same timestamps (a re-import of timestamps still queued) waits for all of it,
  and either side can run out of retries: the build fails, or the import fails before
  storing anything. Both are safe to re-run. The paths without a write-ahead enqueue
  (`capture_new_measurement` and the two chunk methods) still queue after their commit,
  so an error there fails the call although the rows are stored.
- **The incremental build interpolated windows that could not be complete.** A queued
  timestamp past the latest stored row (an append in progress, or the write-ahead
  entries of one that failed or was abandoned, up to a whole call's worth) stays queued
  until rows reach its windows, and every build interpolated each of those windows only
  to find it short. The build now reads the latest stored measurement once per run and
  skips a window that ends after it; the timestamps stay queued as before.
- **Legacy ingest could fail after its rows were stored.** The steps after the commit
  (the second enqueue, the checkpoint, the dirty-region marking, the aspect's
  earliest/latest columns and `capture_measurement`'s transaction-log entry) are now
  best-effort: a failure is logged and the call returns `Ok`, since a client would retry
  an error into duplicate rows.
- **The batch consumer could queue a batch twice.** It rebuilds every window a queued
  timestamp falls in, so after a crash before its dequeue, or once an ingest that ran
  during it queued its committed rows again, it rebuilt windows it had already batched,
  and each became a second pattern occurrence. `insert_unprocessed_batch` and
  `batch_insert_unprocessed_batches` now skip a batch whose `(aspect_id, batch_hash)` is
  already queued, processed, or extracted. Processed batches keep the hash they were
  queued under (it used to be recomputed from the processed measurements), and pattern
  extraction records the hashes of the batches it consumes in a new `extracted_batches`
  table of the processed batch database (a hash and a time per batch, indexed by hash),
  in the transaction that deletes them. That table grows by about one row
  per resolution step of the aspect, since extraction consumes about one sliding-window
  batch per step, so it is bounded: each extraction also deletes the rows older than
  `WEFT_EXTRACTED_BATCH_RETENTION_SECS` (48 hours by default: about 2,880 rows for a
  minute-resolution aspect, 172,800 for a second-resolution one), and
  `clear_processed_batches` (which `Pipeline::prepare_data_full_rebuild` calls) deletes
  them all, so a full rebuild still extracts every window again. The rebuilds the record
  guards against come on the first build after an ingest that overlapped a pipeline
  run, or on the re-run after a build crashed; one that comes after the record expired
  (a pipeline run more than the retention after the previous one, or a longer crash
  recovery) extracts those windows a second time. A first-run full rebuild
  (`is_first_run`) now skips the windows extracted within the retention; older ones are
  batched and extracted again, as before. The unprocessed and processed batch databases
  gain an index, `idx_batches_hash` on `batches(aspect_id, batch_hash)`, and
  `extracted_batches` one on `batch_hash`, created when a process first opens them, so
  each check is a point lookup.
- Rows-mode ingest residuals remain until aspects move to the segment store, and are now
  documented on `batch_capture_measurements` and `capture_measurement`: an error mid-call
  leaves the chunks committed before it, and retrying stores those rows again.
- **A downsample served from `.weftpart` sidecars could lose a zero's decimal scale.**
  With a partial-sidecar policy on, a zero `min`, `max`, `sum`, `avg`, `first` or
  `last` came back at scale 0 (`0`) where decoding the segment gives it the column's
  scale (`0.000` on a `ScaledI64 { scale: 3 }` aspect). The values were equal, but
  exact-decimal output differed. `bigdecimal` writes a decimal as its display string,
  which loses the scale of a zero and of a decimal with a small negative scale (`5E+2`
  is written `500`), so the sidecar lost it. A `PartialReduction` now writes those
  decimals in scientific form (`0e-3`, `5e2`) and every other one as before. The frame
  layout and version (v3) are unchanged, and a `bigdecimal` reader, 0.1.0's included,
  parses the new form exactly. A sidecar 0.1.0 wrote for a `ScaledI64` or `ScaledI128`
  segment gets the segment's scale back on its zeros when it is loaded. One it wrote for
  a segment of another physical type still reads back with those scales lost: nothing
  rebuilds it unless maintenance rewrites the segment.
- **A checked-out v0.1.0 fixture was not the tree the release wrote.** Rows-mode gave every
  aspect an empty `dictionaries/` directory beside its `measurements.db`
  (`v0.1.0:weftdb/src/types/aspect.rs:252`), and an empty directory is the one artifact git
  cannot store. `MANIFEST.sha256` hashes files, so it names none of them and reports no gap
  when they are absent: the 109 files came back complete from a fresh checkout — the negation
  that recovered what `.gitignore` ate accounts for every one — and the three directories did
  not. The readable leg of the rows-mode work names what it leaves behind, and one path it must
  name is `plant/price/dictionaries`, so `weftdb/tests/compat_v010.rs` failed on all three CI
  platforms and passed on any machine that had run the generator: evidence that depended on the
  machine reading it. The test now restores the directories on the copy it reads, one beside
  every `measurements.db` it finds, derived from the tree rather than from a list of aspects —
  so a fixture that grows an aspect grows its directory without anyone editing the test, and the
  committed tree stays exactly the bytes the release wrote.

### Security

- **Backup and drill labels starting with `.partial-`, `.deleting-` or
  `.restore-drill-` are rejected with `400`**: those names belong to unfinished
  backups, prunes and drills, which are never restored and are swept.
- **The backup label grammar is fixed for 1.0: `[A-Za-z0-9][A-Za-z0-9._-]{0,99}`, not
  ending with `.`.** ⚠️ `POST /api/v1/storage/backup?label=` and
  `POST /api/v1/storage/restore/drill?label=` answer `400` for a label that starts with
  `-` or `_` or is longer than 100 characters; both were accepted before. The drill still
  accepts a daemon snapshot's `backup-<digits>`. An existing backup whose name falls
  outside the grammar can be drilled once renamed into it (not into `backup-<digits>`,
  which retention prunes).

## [0.1.0] - 2026-10-08

First public release, and the baseline that later releases' upgrade, rollback and
compatibility checks run against. WeftDB is pre-beta: the API will change, and the
version number says so. This release ships as pre-built binaries on the GitHub release
only; the library crates are not published to crates.io (build them from the `v0.1.0`
tag).

The Changed, Fixed and Security entries are for anyone who built WeftDB from source
before this release. A bare "0.1" in them means splimes 0.1.

### Known limitations

- **Pre-beta.** The HTTP API, the Rust API, the configuration and the on-disk format can
  change in any 0.x release. Nothing is covered by a stability promise before 1.0.
- **No authentication, authorization or TLS.** `weft-server` serves an unauthenticated,
  plain-HTTP API; see
  [`SECURITY.md`](https://github.com/basic-automation/weftdb/blob/v0.1.0/SECURITY.md).
  Do not expose it to an untrusted network: keep the default loopback bind, or put it
  behind something that terminates TLS and authenticates callers.
- **Crash consistency is incomplete.** A `2xx` from a storage ingest survives a crash of
  the `weft-server` process, but not necessarily a power loss or an operating-system
  crash: the index commit is fsynced, the segment frame it points at is not.
  Maintenance (reconcile, split, squash, compaction) still rewrites committed frames in
  place, so a crash during a rewrite can tear one. And two seals into one aspect at the
  same moment can be given the same segment id, so that one replaces the other.
  [`docs/design/crash-consistency.md`](https://github.com/basic-automation/weftdb/blob/v0.1.0/docs/design/crash-consistency.md)
  lists the 43 windows it verified. This release closes one of them, the deleted MVCC
  log (see Fixed). The durability work after it closes or mitigates the rest, though a
  legacy (rows-mode) database keeps some of them by design.
- **The next release changes the store layout.** It moves a store to layout v2, in
  place, and the upgrade is one-way: once a newer release has written to a store, do not
  run v0.1.0 on it again. Before upgrading, stop `weft-server` and copy the whole store
  directory; `POST /api/v1/storage/backup` copies only the control plane, not the
  segment frames.
- **The Linux binaries need glibc 2.34 or newer, and `weft-tui` needs 2.39 or newer.**
  They are built on Ubuntu 24.04. Ubuntu 22.04, Debian 12 and RHEL 9 run `weft-server`
  and `weft-bench` but not `weft-tui`; on an older glibc, build from source.
- **The macOS and Windows binaries are not signed.** Gatekeeper and SmartScreen warn
  before running them. Check each archive against `SHA256SUMS` instead.
- **The bit-sliced codec is off in the release binaries.** None of them is built with
  `bitsliced-codec`, so they never write the bit-sliced value codec
  (`WEFT_SEGMENT_TRANSPOSED_MAX_OVERHEAD` is ignored, with a warning) and cannot read a
  segment written with it; for a store that has such segments, build `weft-server` from
  source with `--features bitsliced-codec`. `experimental-codecs` is compiled into
  `weft-bench` only, for its advisory size estimates; nothing writes those codecs to
  disk.

### Added

- **Interpolation as a first-class query.** Ask for any resolution and get back a
  continuous series, reconstructed with the spline method you choose (linear,
  quadratic, cubic, polynomial), computed on SIMD/parallel CPU or GPU (`wgpu`).
- **Provenance labelling.** Every returned point is marked `raw`, `interpolated` or
  `extrapolated`, so a synthetic value is never silently mistaken for an observed one.
- **Declared precision.** Values are logically `BigDecimal`. Each aspect declares a
  physical encoding (`F64`, `F32`, `ScaledI64`, `ScaledI128`, `Decimal128`,
  `BigDecimalText`) and an error bound; a value the encoding cannot represent within
  that bound is rejected rather than quietly rounded.
- **Typed columnar segment store** (`.weftseg`) on the measurement hot path, with
  bit-packing and delta-of-delta timestamp coding (an opt-in bit-sliced value codec sits
  behind the `bitsliced-codec` feature; see Changed), over a Turso (libSQL) control
  plane for catalog, metadata and the segment index.
- **Downsampling** with mergeable partial reductions (`.weftpart` sidecars), including
  time-weighted averages, over an epoch-aligned bucket grid.
- **Apache Arrow interchange** — sealed segments to `RecordBatch` and back, plus Arrow
  IPC and Parquet bytes.
- **HTTP API** (`weft-server`) covering health/readiness, ingest, query, interpolation,
  storage management, backup/restore and a restore drill, with OpenTelemetry tracing.
- **Terminal UI** (`weft-tui`) for exploring and administering an instance.
- **Weft-Bench** (`weft-bench`), a reproducible, correctness-gated benchmark harness.
- **Analytics pipeline** (`weft-orchestration`) chaining batching, pattern extraction,
  event detection, correlation and signal generation.
- Pre-built `weft-server`, `weft-tui` and `weft-bench` archives for Linux (x86_64 and
  aarch64), macOS (x86_64 and arm64) and Windows (x86_64), with a `SHA256SUMS` file,
  attached to the GitHub release.
- **Interpolation grids requested over HTTP are capped** at 10,000,000 points per
  request by default (`weft_server::MAX_INTERPOLATE_OUTPUT_POINTS`), on every
  `/api/v1/interpolate*` endpoint; set `WEFT_MAX_INTERPOLATE_POINTS` to change it. The
  variable is read once at startup, and a value that is not a positive integer stops
  `weft-server` from starting. A larger grid is a `400` that names its size and the
  limit, refused before anything is allocated; before, a fine resolution over a long
  range tried to allocate all of it.
- **`weft-server` calibrates the interpolation backends at startup.** Once the listener
  is bound, it runs `splimes::calibrate()` once in the background on a blocking thread:
  that starts the GPU if there is one, times the single-thread, rayon and GPU backends,
  and sets where `Backend::Auto` switches between them. It takes several seconds and
  never delays serving or fails startup; until it finishes, requests interpolate on the
  CPU with splimes' default thresholds. The adapter (or why there is none) and the
  thresholds are logged. A CPU/software adapter (llvmpipe, lavapipe, WARP) is not
  calibrated unless `WEFT_GPU_CALIBRATE=force`; `WEFT_GPU_CALIBRATE=0` skips calibration.
  Without it, splimes 1.0 never uses the GPU.
- **Weft-Bench calibrates like `weft-server`.** An interpolation run (line protocol or
  `--synthetic`) calls `splimes::calibrate()` once before anything is timed, skipping a
  CPU/software adapter as the server does (it has no counterpart to
  `WEFT_GPU_CALIBRATE=force`), so the numbers come from the engine the server runs on
  that machine; `--no-gpu-calibrate` turns it off. The calibration, the GPU and
  `Backend::Auto`'s thresholds are printed and recorded in the report's
  `metadata.engine` (bench schema v16) and its HTML view.
- **Third-party attribution.** A root `NOTICE`, and a `THIRD-PARTY-NOTICES` file shipped in
  the `weft-physical-type` and `weft-reduce` packages, credit the two Apache-2.0 projects
  whose code is adapted here: the Chimp/Chimp128 codecs (from the authors' reference
  implementation) and the DDSketch quantile sketch (from Datadog's sketches-java), with the
  upstream NOTICE text and the license. The Chimp128 docs no longer credit DuckDB as the
  source.
- **Contribution terms.** Every pull request takes one of two routes, the contributor's
  choice: a DCO sign-off (`git commit -s`) on every commit, or the WeftDB Individual
  Contributor License Agreement
  ([`CLA.md`](https://github.com/basic-automation/weftdb/blob/v0.1.0/CLA.md), version 1,
  adapted from the Apache Software Foundation's ICLA with Justin Icenhour as the
  recipient), signed once by a pull request comment. The `contribution-terms` check
  passes a pull request when either holds. Contributions are licensed
  `MIT OR Apache-2.0`. See
  [`CONTRIBUTING.md`](https://github.com/basic-automation/weftdb/blob/v0.1.0/CONTRIBUTING.md#contribution-terms).
- **`Inputs::register_dictionary_if_absent`** registers a dictionary as
  `set_dictionary_metadata` does unless a complete registration of that name exists,
  checking inside its own write transaction, and returns whether it registered it. A
  registration that cannot be read counts as existing and is left alone, and a write
  that loses an MVCC conflict is tried once more after a short random delay. The check
  sees registrations committed before its transaction began: one committed while it
  runs, like a second writer registering the same new dictionary at the same moment, is
  not seen, both are written, and reads pick the newest.

### Changed

- **Crates renamed for publication.** The core crate is now `weftdb` (was `database`,
  a name already taken on crates.io) and the pipeline crate is `weft-orchestration`
  (was `database_orchestration`). Import paths change accordingly:
  `use database::…` becomes `use weftdb::…`, and `use database_orchestration::…`
  becomes `use weft_orchestration::…`.
- **The workspace builds on stable Rust** (MSRV 1.95). The
  `#![feature(stmt_expr_attributes)]` gate is gone, so nightly is no longer required to
  build, test or depend on any crate. Only `cargo fmt` still uses nightly, because
  `rustfmt.toml` sets nightly-only options.
- **`splimes` moved to its own repository** ([basic-automation/splimes](https://github.com/basic-automation/splimes))
  and is now a crates.io dependency. It is released on its own schedule, and a stable
  WeftDB waits on a stable splimes.
- **splimes 0.1 → 1.0.** WeftDB depends on `splimes = "1"`, resolved to 1.0.0 from
  crates.io. splimes' [migration guide](https://github.com/basic-automation/splimes/blob/main/MIGRATING.md)
  lists the results that change; through WeftDB:
  - large inputs keep their method (0.1 swapped cubic for quadratic from 2,500 input
    points, and cubic and quadratic for linear above 5,000);
  - `Polynomial(1 | 2 | 3, b)` stays a polynomial, so it extends outside the data
    instead of holding flat, and with too few points a polynomial steps down to degree
    `n − 1`. 0.1 stepped down to `Cubic`/`Quadratic`/`Linear` with four input points or
    fewer, and with five or more kept the requested degree and failed;
  - a polynomial degree above 8 is an error instead of being capped at 8;
  - a zero-span range returns its one point: `POST /api/v1/interpolate` with a single
    input point and no explicit range answers `200` with that point (it was a `500`);
  - points that coincide with an input return that input's value exactly, and
    interpolated `BigDecimal`s are the shortest decimal that round-trips;
  - time is exact to the nanosecond at every resolution, duplicate timestamps keep the
    last value, and a leap second is the same instant as the start of the next second;
  - results no longer depend on whether the GPU ran them. With a GPU present, 0.1's
    `auto_interpolate` (behind `analyze_range`, `analyze_point`, compression and every
    `weft-server` interpolation endpoint) ran a call on it when the larger of its input
    count and grid size was 100–999, or either reached 50,000. Its GPU path evaluated a
    polynomial of degree 0 or 4–7 one degree higher than the CPU did (both capped at
    `n − 1` and 8) and, in `f64`, ignored any bounds factor other than 1.0, so those
    calls returned different polynomial values from the CPU's, and on a GPU without
    `f64` support it computed in `f32`. 1.0 computes the same method, in `f64`, on every
    backend;
  - `Backend::Auto` uses the GPU only once the program has started it, and only above
    the calibrated thresholds (see the startup calibration above); without calibration
    every interpolation runs on the CPU.
- **No interpolation runs on an async worker.** splimes 1.0 is synchronous; `weftdb`'s
  `analyze_point`, `analyze_range` and compression, every `weft-server` interpolation
  endpoint, and the Weft-Bench WeftDB adapter run it on tokio's blocking pool
  (`Interpolator::run_async`, splimes' `tokio` feature). 0.1's `async` functions
  computed on the calling worker.
- **Downsample buckets don't move.** splimes 1.0 removed `Resolution::to_base` and the
  `SECONDS_IN_*` constants; the new `weft_reduce::bucket_index` reproduces 0.1's index
  exactly, including its rounding toward zero before 1970, so bucket edges and the keys
  of persisted `.weftpart` sidecars are unchanged. `Resolution::difference` is replaced
  by `weftdb::units_between`, with the same results.
- **Breaking for Rust users: `weftdb::Resolution` and `weftdb::Spline` are
  `#[non_exhaustive]`.** They are re-exports of splimes' types, which 1.0 marks
  non-exhaustive so new variants can arrive in minor releases: an exhaustive `match` on
  either needs a `_` arm. Their removed 0.1 helpers go with them: use
  `Resolution::step()`/`step_nanos()` for `to_step()` and the `SECONDS_IN_*` constants,
  `weftdb::units_between` for `difference`, `weft_reduce::bucket_index` for `to_base`,
  and `Spline::min_points()` for `number_of_points_required()`. `Resolution::round()`
  and `to_step_base()` need no replacement: a step is one unit at every resolution, so
  `to_step_base()` was always `1` and `round()` returned its input unchanged (or, at
  nanosecond resolution outside 1677–2262, an error). `Spline::pre_check()` has no
  direct replacement: `Spline::validate()` checks a polynomial's parameters, and the
  engine checks the range and steps down when there are too few points (or, under
  `Interpolator::exact(true)`, returns `InsufficientPoints`).
- **Stored spline methods are validated when they are loaded.** A dictionary created
  with `AspectStructure::new_dictionary`, or registered by a pipeline through
  `weft_orchestration::load_dictionary` (which stored no constraints before this
  release; see Fixed), persists its step interpolation as the `Spline`'s text
  (`steps_interpolation` in the `dictionary_constraints` table of
  `<aspect>/dictionaries/<dictionary>.db`). splimes 1.0's `Spline::from_str` validates what it parses, so a stored
  `Polynomial` with degree 0, a degree above 8, or a negative or non-finite bounds
  factor, which 0.1 accepted, no longer loads: `get_dictionary_metadata` returns
  `Database error: Invalid interpolation format: invalid polynomial degree 9: must be
  between 1 and 8` (or `… invalid bounds factor -1: must be finite and not negative`),
  and `load_dictionary` logs that error as a warning ("Failed to read dictionary
  metadata; leaving the stored registration as it is") and carries on with the
  pipeline's own constraints, without touching what is stored. Until this release `get_dictionary_metadata` could
  not read any stored dictionary (see Fixed), so this is the first release that reads,
  and so checks, a stored method at all. To fix it, stop WeftDB and rewrite the stored
  value with Turso's shell (`tursodb`, not `sqlite3`: the file is in Turso's MVCC
  journal mode), e.g. `UPDATE dictionary_constraints SET steps_interpolation =
  'Polynomial(degree: 8, bounds_factor: None)' WHERE steps_interpolation =
  'Polynomial(degree: 9, bounds_factor: None)'`; 0.1 capped any degree above 8 at 8.
  Degree 0 and invalid bounds factors have no exact 1.0 equivalent: choose a degree
  from 1 to 8 and a finite, non-negative bounds factor, or `None`. `new_dictionary`
  now checks the method with `Spline::validate()` and refuses one that would not load,
  e.g. `Invalid step interpolation for dictionary 'd': invalid polynomial degree 9: must
  be between 1 and 8`; it stored any method before. `set_dictionary_metadata`, and so
  `load_dictionary` when it registers a dictionary, refuses one the same way.
- `Resolution` names parse case-insensitively (`Hours`, `HOURS`), for example in
  `WEFT_SEGMENT_PARTIAL_BASE`, which ignored anything but lowercase before.
- **`weft-server` provenance comes from splimes.** The `kind` of each interpolated
  point (JSON, CSV, Arrow, Parquet, and the point query) is splimes' `PointKind`, with
  the same `raw` / `interpolated` / `extrapolated` tokens, and `weft_server::PointKind`
  is now a re-export of it. It agrees with the classification `weft-server` did itself
  except at a leap second: an input at `23:59:60.5` and a grid instant at `00:00:00.5`
  the next day are one POSIX instant, so that point is now `raw` (it was `extrapolated`
  or `interpolated`).
- **Invalid polynomial parameters are a `400`.** A `polynomial` spline with degree 0, a
  degree above 8, or a negative `bounds_factor` used to be accepted, and the
  interpolation endpoints usually answered `200` with a result (0.1 capped a degree
  above 8 at 8). Not always: with 5 to `degree` input points a degree above 8 was a
  `500` (0.1's failed step-down, above), and on a machine without a usable GPU, a
  request whose larger of input count and grid size was 100–999 or 50,000 and up ran
  0.1's SIMD path, where degree 0 was a `500` and a bounds factor below −0.5 panicked
  once it extrapolated non-constant data, dropping the connection without a response.
  splimes 1.0 rejects them, and the request is now a `400` naming the problem, e.g.
  `invalid polynomial degree 9: must be between 1 and 8`. (JSON cannot carry a
  non-finite bounds factor; such a body is a `400` from the JSON parser before splimes
  sees it.)
- **Interpolation errors map to HTTP status by kind.** An input or extrapolated value
  beyond `f64`'s range and an oversized grid are `400`s too; any other engine error
  (a GPU failure, a panicked blocking task) stays a `500`, as every engine error was
  before.
- **Store-wide maintenance carries on past a failing aspect.** Every store-wide sweep
  (reconcile, overlap merge, squash and compaction, from the background daemon or
  `POST /api/v1/storage/reconcile`) stopped at the first aspect whose pass failed, on a
  torn or truncated frame for example, and so never maintained the aspects after it in
  name order. Each aspect's failure is now its own: the endpoint answers `200` and lists
  the aspects that failed in `failed: [{aspect, error}]` (it answered `500`), the daemon
  logs one `WARN` line per failed aspect, and both count them in
  `weft_reconcile_failed_passes_total`. Only an unreadable aspect list is still a `500`.
  For Rust callers, `ReconcileSweep`, `HotColdSweep`, `OverlapSweep` and `SquashSweep`
  gain `failed: Vec<(String, anyhow::Error)>` and are no longer `Copy`, `Clone`,
  `PartialEq` or `Eq`.
- **Turso control plane upgraded 0.6 → 0.8.** ⚠️ This is one-way: once 0.8 writes a
  store, its MVCC log is v3 and an older WeftDB can no longer open it. Back up the
  control plane before upgrading.
- **`.weftpart` sidecars use `postcard` instead of `bincode`** (frame v3). Sidecars
  written by older versions are ignored: the segment is decoded instead, and the
  sidecar is rebuilt on the next seal. No data migration is needed.
- `weft-bench --spline poly:N` rejects a degree outside 1–8 when the arguments are
  parsed, naming the limit. It accepted any degree before, and with splimes 1.0 such a
  run would only have failed once the engine rejected it.
- All dependencies updated to their latest major versions, including wgpu 30,
  Arrow/Parquet 60 and OpenTelemetry 0.33.
- **Advisory codecs moved behind the `experimental-codecs` feature** (`weft-physical-type`).
  ⚠️ Breaking for code that calls them: the `floatcodec` module (Gorilla-XOR, Chimp,
  Chimp128, Elf and `best_f64_*`), `ColumnEncoding::{gorilla_f64_bytes, best_f64_bytes,
  best_f64_codec}` and the FIRE forecaster (`fire_*`) now need
  `features = ["experimental-codecs"]`. None of them was ever written to disk, so stored
  segments are unaffected. The feature is off by default, outside the semver promise, and
  pending patent review.
- **The opt-in transposed value codec is now the `bitsliced-codec` feature**
  (`weft-physical-type`, forwarded by `weftdb` and `weft-server`), pending patent review.
  ⚠️ A store that enabled it with `WEFT_SEGMENT_TRANSPOSED_MAX_OVERHEAD` must be built with
  `bitsliced-codec` to read those segments: without the feature, reading one fails with the
  new `WeftSegError::CodecNotEnabled` (which names the feature) instead of decoding, and the
  variable is ignored with a warning. For library callers, without the feature
  `FrameOptions::transposed_max_overhead` is accepted but silently ignored
  (`write_segment_with` and `write_paged_segment_with` emit the size-selected codec), and
  `WeftSegError` gains the `CodecNotEnabled` variant, which breaks exhaustive matches on it.
  The `transpose_bitpack_*` primitives, `TRANSPOSE_TILE`,
  `ColumnEncoding::{transposed_value_bytes, transposed_overhead, best_value_codec_transposed}`
  and `weftseg::write_value_column_transposed` need the feature too. The default
  configuration never wrote this codec, so a store that never set the variable (or
  `FrameOptions::transposed_max_overhead`) is unaffected. The docs now call it a bit-sliced
  (bit-plane-major) layout; it is not the FastLanes layout they used to name.
- **WeftDB is dual-licensed under MIT OR Apache-2.0**, at your option, from this release on.
  Every crate declares `license = "MIT OR Apache-2.0"`, and the repository root and every
  crate ship `LICENSE-MIT` and `LICENSE-APACHE` in place of `LICENSE`. The release archives
  carry both files, `NOTICE` and the `THIRD-PARTY-NOTICES` files. Earlier commits remain
  available under MIT, as they were published. The portions adapted from Chimp and
  DDSketch stay under Apache-2.0 whichever option you choose.
- The MIT license text (now `LICENSE-MIT`) reads `Copyright (c) 2025-2026 Justin Icenhour`,
  naming the individual copyright holder for both years of the project.

### Fixed

- **A restart lost a legacy database's committed metadata.** The first open of a
  legacy (rows-mode) database in a process deleted its Turso MVCC log, `<file>.db-log`.
  Under MVCC that log holds every committed transaction until a checkpoint copies it
  into the main file, and WeftDB's checkpoints do not run under MVCC, so for
  `metadata.db` it was usually the only copy: every commit made before a restart (the
  database row, subjects, aspects, the unbatched queue) was lost, and
  `Database::existing` then failed with `no such table: database`. A non-empty log is
  now kept, and Turso replays it at open; only an empty one is removed.
- **Interpolation responses report the method that ran.** `spline` in the
  `/api/v1/interpolate` and `/api/v1/interpolate/ilp` JSON responses and in the point
  query is documented as the method actually used, but echoed the requested one. With
  fewer distinct timestamps than a method needs, splimes steps down (`Cubic` →
  `Quadratic` → `Linear`, `Polynomial(d, b)` → `Polynomial(n − 1, b)`), so a `cubic`
  request over three points ran a quadratic and still said `Cubic`; it now says
  `Quadratic`. The CSV, Arrow and Parquet outputs carry no method field. The
  `interpolate.engine` trace span keeps the requested method in `spline` and now
  records the one that ran as `effective_spline`.
- **`get_dictionary_metadata` and `list_dictionaries` failed for every stored
  dictionary.** Both selected an `updated_at` column that the `dictionary_metadata`
  table does not have (`Failed to query dictionary metadata: … no such column:
  updated_at`), and `get_dictionary_metadata` then read `steps_count`, an `INTEGER`
  column, as text. They read the table as it is now: the count as an integer (text is
  still accepted), and a dictionary without steps as `None`. The metadata
  `get_dictionary_metadata` caches is keyed by aspect as well as name, so two aspects'
  dictionaries of the same name no longer share it.
- **A pipeline's dictionaries lost their steps and variabilities, and gained a metadata
  row on every load.** `set_dictionary_metadata`, which
  `weft_orchestration::load_dictionary` registers a pipeline's dictionaries with,
  inserted only the `dictionary_metadata` row. The steps and variabilities were never
  stored, `get_dictionary_metadata` (which needs the constraints row) never found the
  dictionary, and so every `load_dictionary` (two per `Pipeline::extract_patterns`)
  registered it again, with another row and a new id. It now writes the whole
  registration and replaces any earlier one of that name, since the tables cannot carry
  a unique constraint; `AspectStructure::new_dictionary` writes the same way, so creating
  a dictionary twice no longer leaves two. `load_dictionary` registers a dictionary only
  when none is registered, under the `Dictionary`'s own id, through the new
  `Inputs::register_dictionary_if_absent`: it checks again inside its write, so a
  registration committed after `load_dictionary` read none and before that write began
  (an explicit `set_dictionary_metadata` with tuned constraints, say) is kept instead of
  replaced with the pipeline's, and a write that loses an MVCC conflict to another load
  healing the same rows is tried once more. A dictionary name that cannot name a file is an
  error from `load_dictionary`. `get_dictionary_metadata`
  now answers `Ok(None)` for a dictionary with no database yet (it was an error,
  `Dictionary '…' does not exist for aspect '…'`), and a registration it cannot read
  is left alone instead of registered again. Of several rows for one name,
  `get_dictionary_metadata` reads the newest that has constraints (it read whichever
  came first), so a dictionary an earlier version registered from a pipeline reads as
  unregistered and is registered again, with its constraints, on its next load, which
  also deletes its extra rows.
- **`get_dictionary_metadata` returned no variabilities.** It always answered `None`,
  although `new_dictionary` stores them. It now reads them back in the order they were
  stored; an empty list stores nothing, and so reads back as `None`.
- **`list_dictionaries` read only a dictionary named "default".** It opened the
  aspect's dictionary named "default", so it failed for an aspect without one
  (`Dictionary 'default' does not exist for aspect '…'`) and otherwise listed only that
  dictionary. Each dictionary is its own `<aspect>/dictionaries/<name>.db`; it now lists
  all of them, by name, each with its steps and variabilities (it returned empty
  constraints), and the list is no longer cached, so it does not go stale when a
  dictionary is added. Only regular files are listed: a symlink, which it followed and
  so opened (setting the target's journal mode and creating tables in it), a directory
  or another entry named `*.db` is skipped. Every regular `*.db` file there is still
  opened as a dictionary, so keep backups out of that directory or under another
  extension. A dictionary whose stored registration does not parse, such as one with a
  stored step method splimes rejects, is logged as a warning and left out instead of
  failing the whole listing; a read that fails (I/O, a query, an MVCC conflict) still
  fails it, so a dictionary is never left out only because it could not be read.
- **A dictionary file without its tables could not be registered.** A dictionary's file
  can exist before its tables, for example when `insert_pattern_into_dictionary` opened
  it first, which creates no tables. Once the file was open in the process nothing created
  them, so `get_dictionary_metadata` failed with `no such table: dictionary_metadata`,
  `load_dictionary` only warned and never registered the dictionary, and
  `list_dictionaries` failed for the whole aspect. Such a file holds no registration, so
  it now reads as unregistered (`Ok(None)`), and registering the dictionary creates the
  tables.
- **A metadata read could cache a registration that had just been replaced.** A
  `get_dictionary_metadata` whose snapshot predated a `set_dictionary_metadata` commit
  could store the old registration after the write had invalidated it, and the old one
  was then served for up to the ten-minute cache lifetime. A read now caches what it read
  only if no write on the same `Database` invalidated the entry since it began
  (`DatabaseCache::generation`, `invalidate_generation` and `store_if_generation`). A
  registration written through `AspectStructure::new_dictionary`, another `Database`
  handle or another process still does not invalidate the cache, and can be served
  stale until the entry expires.
- **Commit failures were silently ignored.** A control-plane write that lost an MVCC
  conflict was reported as success. Commit errors now reach the caller, and the
  transaction is rolled back.
- **A write that lost an MVCC conflict reported a failed rollback instead.** Turso rolls
  a transaction back itself when one of its statements loses a write-write conflict, so
  the `ROLLBACK` after the failed statement fails (`cannot rollback - no transaction is
  active`), and control-plane writes (registering or creating a dictionary, inserting a
  pattern, removing an event, …) returned that error, `Rollback failed: …`, in place of
  the conflict. They now return the statement's error and log the failed rollback as a
  warning. `weftdb::error::is_transient_mvcc_error` also matches Turso's `Busy`,
  `BusySnapshot` and write-write conflict errors anywhere in an error's chain (it
  matched only WeftDB's own `TransientMvccError`), so it recognises the conflict as
  retryable where the returned error keeps Turso's error in its chain: from registering
  or creating a dictionary (`set_dictionary_metadata`, `register_dictionary_if_absent`,
  `AspectStructure::new_dictionary`) and from a failed commit. The other control-plane
  writes still return the statement's error as text only, which it does not recognise.
- **Quadratic GPU interpolation failed on GPUs without f64 support** (Apple Silicon,
  most integrated GPUs, Windows WARP). The f32 fallback shader did not parse, so wgpu
  panicked.

### Security

- **Aspect names can no longer point outside the segment store.** An aspect's name is
  part of the file names of its `.weftseg` frames and `.weftpart` sidecars, and it was
  not checked, so a client of `weft-server` could declare a name that made sealing,
  reconciling or compacting create, overwrite or delete those files outside the store's
  `segments/` directory. Names are now validated wherever they are declared or turned
  into a path (`weftdb::aspect_name::validate`): at most 160 bytes, no `/` or `\`, no
  control characters, no leading `.`, no trailing `.` or space, and not a Windows device
  name (`CON`, `NUL`, `COM1`, `CONIN$`, …, also with an extension or a `:` suffix).
  Bidirectional controls, line and paragraph separators and invisible formatting
  characters are refused as well, so a name cannot display as a different one in
  listings and logs (the zero-width joiner and non-joiner, which some scripts need, are
  still accepted). On
  Windows, `<`, `>`, `:`, `"`, `|`, `?` and `*` are refused too, since such a name could
  never be sealed there. `POST /api/v1/storage/aspects` answers `400` for an invalid
  name, and every frame path is also checked to be a direct child of `segments/`. An
  aspect already declared under such a name was never safe to use: sealing, reading or
  maintaining it now fails with a typed `weftdb::InvalidAspectName` error (a `400` from
  the ingest, read, reconcile, squash and compact endpoints), and the store-wide
  maintenance sweeps list it in `failed` and count it in
  `weft_reconcile_failed_passes_total` while still maintaining every other aspect.
  Listing it and reading its schema or stats, which touch no files, still work. Names
  that differ only in letter case or Unicode normalisation are still accepted and can
  share frame files on a case-insensitive or normalising filesystem; that is a collision
  inside `segments/`, not a way out of it, and the planned encoded frame names remove it.
- **Dictionary names can no longer point outside the aspect's `dictionaries/`
  directory.** Each dictionary is its own `<aspect>/dictionaries/<name>.db`, and the name
  was not checked, so `../x` reached `<aspect>/x.db` and an absolute name such as `/tmp/x`
  replaced the whole path; `get_dictionary_metadata` then created that file and its
  tables, and `new_dictionary` and `set_dictionary_metadata` wrote to it. Dictionary
  names now follow the aspect-name rules (`weftdb::dictionary_name::validate`), checked by
  the dictionary path builders, so every dictionary operation (`new_dictionary`,
  `Aspect::dictionary`, `set_dictionary_metadata`, `insert_pattern_into_dictionary`,
  `get_dictionary_metadata`, `get_dictionary_db`, `get_dictionary_patterns`) refuses
  such a name with a typed `weftdb::InvalidDictionaryName` error before any file is
  touched. `Config::aspect_dictionaries_db_path` and `Config::dictionary_path` return
  `Result<String>` (they returned the path), and `list_dictionaries` skips a `.db` file
  whose name is not a valid dictionary name, with a warning. No `weft-server` endpoint
  takes a dictionary name.

  **Breaking for existing data:** a dictionary an earlier version created under a name
  these rules now refuse (a leading `.`, a trailing `.` or space, a Windows device name
  on any OS such as `aux`, `con`, `com1` or `nul.x`, a control, bidirectional or
  invisible formatting character, or more than 160 bytes) can no longer be reached.
  Every operation on it returns `InvalidDictionaryName`, `load_dictionary` fails the
  pipeline run that uses it, and `list_dictionaries` leaves it out with a warning.
  Nothing is deleted. To recover one, stop everything that uses the database, rename
  `<aspect>/dictionaries/<name>.db`, and the `<name>.db-log` and `<name>.db-wal` files
  beside it if present, to a valid name, then set that name in the `name` column of the
  dictionary's `dictionary_metadata` rows, in the `dictionary_name` column of the
  `pipeline_dictionaries` rows in the aspect's `pipeline.db`, and in the pipeline
  configuration (`PipelineRunConfig`, `PipelineBuilder`) that names it.
- **API callers can no longer make backup retention delete the daemon's snapshots.**
  `WEFT_BACKUP_KEEP` retention keeps the newest `backup-<digits>` directories by their
  embedded timestamp, and `POST /api/v1/storage/backup` created directories in that same
  form, both for a `?label=` in it and for every unlabelled backup, so a caller could fill
  the retained set and get the daemon's genuine snapshots pruned. That form is now
  reserved for the backup daemon: a `?label=` in it is a `400`, and an unlabelled backup
  is now named `manual-<unix_millis>`. A label also may no longer start or end with `.`
  (Windows drops a trailing dot from a directory name, so such a label could still land
  on a reserved name there); the restore drill's `?label=` follows the same rule.
  Snapshots taken through the endpoint are therefore never counted or pruned by
  retention; this is a behaviour change for unlabelled backups, which used to be pruned,
  so remove them by hand when no longer needed. Retention also ignores a
  generated-looking directory whose stamp is more than 24 hours past the current clock or
  past the directory's own modification time, which no daemon snapshot ever is: it is
  not counted and not removed, and each listing logs a warning naming it. A directory
  planted through the API before this release keeps its planting time as its
  modification time, so a far-future stamp stays ignored after the clock reaches it, as
  long as the directory is not modified and its filesystem reports modification times.
  One stamped less than 24 hours past its planting looks like a snapshot from a skewed
  clock and is counted, but no longer outranks new snapshots a day after it was
  planted. Remove such directories by hand. Anyone who can write to the backup directory
  directly can still affect retention; this closes the API path.
- **Web pages can no longer drive a loopback-bound `weft-server`.** With no
  authentication, the default `127.0.0.1` bind was the only protection, but a page open
  in a browser on the same machine could still send requests that need no CORS
  preflight (enough to trigger maintenance, backups and ingest), or reach the API
  through DNS rebinding. While bound to a loopback address (including the IPv4-mapped
  `[::ffff:127.0.0.1]`), the server now answers only requests addressed to `localhost`,
  `127.0.0.0/8` or `[::1]` (`421 Misdirected Request` otherwise, `/health` and `/ready`
  included) and refuses a state-changing request whose `Origin` is not a loopback origin
  (`403`). Clients that send no `Origin`, such as `curl`, are unaffected. Pages served by
  another local web server (any loopback origin, on any port) are still trusted until
  authentication lands. Health probes must send a loopback `Host` such as `localhost` or
  `127.0.0.1`, as every HTTP/1.1 client does; an HTTP/1.0 probe that sends no `Host` now
  gets `421`, so configure it to send one or set `WEFT_ALLOW_ANY_HOST=1`. Behind a local
  reverse proxy that forwards a different `Host`, or the non-loopback `Origin` of a
  browser UI it fronts, set `WEFT_ALLOW_ANY_HOST=1`. Non-loopback binds are unchanged
  and remain unauthenticated.
- Cleared the `crossbeam-epoch` (RUSTSEC-2026-0204) and `h2` (RUSTSEC-2026-0258)
  advisories, replaced the unmaintained `bincode` (RUSTSEC-2025-0141), and removed the
  unsound `lru` 0.16 (RUSTSEC-2026-0253) by disabling turso's unused full-text search.

[Unreleased]: https://github.com/basic-automation/weftdb/compare/v0.3.1...HEAD
[0.3.1]: https://github.com/basic-automation/weftdb/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/basic-automation/weftdb/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/basic-automation/weftdb/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/basic-automation/weftdb/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/basic-automation/weftdb/releases/tag/v0.1.0
