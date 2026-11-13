# WeftDB Roadmap

One checklist, in phases. Each phase starts only when the phase before it is done, and nothing in a phase depends on an item in a later one. Item IDs (`7.2`, `6.1`, `S17`, `FRE-9b`, …) are stable and match DECISIONS.md, issues and PRs. Prose, measurements, audit notes and research notes are in [docs/roadmap-notes.md](docs/roadmap-notes.md).

Sub-items run top to bottom under their parent. In Phases 1–11, `→` marks an item that followed the one above it.

Phases 1–11 record shipped work in the order it landed. Phases 12–21 lead to 1.0.0. Phases 22–30 follow 1.0.

## Phase 1 — Benchmark thesis and foundations

- [x] Design-partner stand-in datasets chosen ([DESIGN-PARTNERS.md](DESIGN-PARTNERS.md))
- [x] Willingness-to-pay hypotheses documented ([WILLINGNESS-TO-PAY.md](WILLINGNESS-TO-PAY.md))
- [x] Publish a methodology document **before** any performance claim
- [x] `axum` server with `POST /api/v1/interpolate` over `splimes`, ILP ingest, downsample/aggregation, point, stored-range and single-instant reads, maintenance, HTTP catalog with batch ingest
- [x] Parquet and CSV in, Arrow/Parquet out, cursor paging on stored-range reads
- [x] Prometheus latency histograms, OpenTelemetry trace export and `/metrics`
- [x] `splimes` 1.0 released and depended on
- [x] Control-plane engine decision: Turso 0.4 → 0.6 → 0.8.1, plain `INTEGER PRIMARY KEY` for MVCC, 0.7 evaluation settled
- [x] Online control-plane backups over `VACUUM INTO`, concurrent-write-safe verify, background daemon, no stray sidecars
- [x] Restore and the backup/restore drill, exposed at the API
- [x] `Statement::n_change()` row accounting on the aspect-metadata and seal index writes
- [x] Confirm the snapshot verifier does not depend on MVCC-broken pragmas
- [x] **4.1** Time policy: POSIX UTC ticks, no zone, no leap seconds
- [x] **S1** Legacy MVCC logs preserved, no-op sync pragmas dropped
- [x] **S2** Durable I/O layer with fault points and a power-cut simulator
- [x] **S4** Store-wide sweep isolation
- [x] Integer-native reduction path wired into `SegmentStore::downsample_range`
- [x] Read-path arc: streaming and batch point lookup, timestamp search, windowed range reads
- [x] `weft-reduce`: mergeable DDSketch, cross-segment downsample, size-targeted compaction with a fragmentation gate
- [x] DONE (2026-07-18) — materialize per-segment partials at seal time
- [x] Compression v2 codecs through adopt-or-drop and realised on disk (Gorilla+RLE, adaptive bit-packing, scaled-integer bit-pack, FOR, f64 best-of selector, cascades, FastLanes layout)
- [x] Test infrastructure: GPU flakes, rustc ICE and link OOM root-caused; `cargo test --workspace` completes
- [x] Historical note — the LNK1102 research that produced the `[profile.test] debug = 1` fix
- [ ] Months 3–4 — Hot-path optimization

## Phase 2 — Lock the store shapes (was W0)

- [x] **INT-0** Land the splimes-1, legal and durability branches into main
- [x] **S3 / D-S3** Storage-v2 open hardening
- [x] **S5 / D-S5 + SEC-1** Atomic backup publish with manifest, retention and drill hygiene; backup-label grammar
- [x] **S6 / D-S6** Schema v2 with `IndexTxn`, write poison and root-relative frame paths
- [x] **S7 / D-S7** Persistent per-aspect id allocator with commit and maintenance locks
- [x] **S18 / D-S18** Legacy rows-mode hygiene
- [x] Decision 1: the store shapes S6 and S7 rely on
- [x] **PAC-1a** Release pipeline
- [x] **TAG-1** Canonical `TagSet`, `SeriesKey`, `SeriesSelector` with frozen caps
- [x] **Security process**
  - [x] Private vulnerability reporting is off *(owner switch; now on)*

## Phase 3 — Baseline release, server split, goldens (was W1)

- [x] **FRE-1** Per-crate versions, single edition, publish split, package hygiene
- [x] **FRE-3** Golden frames, sidecars and persisted-encoding goldens
- [x] **TAG-3** Open `ValueKind` with a provenance scope
- [x] **EVI-1** README commands, flags, endpoints and links checked
- [x] **EVI-3** Bench run manifest
- [x] **EVI-9** `--sparse` and `--scale` workloads
- [x] **SOA-1** Soak harness skeleton
- [x] **PAC-4** v0.1.0 baseline release (GitHub release only); stale draft prerelease deleted
- [x] **X-FX** Fixture set written by the v0.1.0 binary
- [x] **X-SPLIT-SRV** `manage.rs` and `main.rs` split
- [x] **PAC-5a** Configuration registry skeleton
- [x] **S8 / D-S8** Write-once maintenance I
- [x] **S9 / D-S9** Write-once maintenance II

## Phase 4 — Error contract, frame format, portable builds (was W2)

- [x] **X-ERR** One error envelope and code table
- [x] **X-NAME** `aspect_name::encode` and `validate_new` at declare
- [x] **ROB-2** Storage-engine panic fail-stop
- [x] **X-FRAME** Frame registry with its v7 envelope
- [x] **PAC-1b** Stripped dist profile with a third-party licence bundle
- [x] **PAC-3** Library crates impose no global allocator or wgpu features
- [x] **Crate publishing split**
- [x] **FRE-14c** `weft-server import-rows-mode`
- [x] **PAC-2** Portable Linux binaries on a glibc 2.28 baseline
- [x] **EVI-5** Nightly regression workflow and `weft-bench compare`
- [x] **EVI-2** `bench/claims.json` and `tools/check_claims.py` in CI
- [x] **EVI-4** Artifact pipeline
- [x] **EVI-6** Ingest workload over SegmentStore with the UCI loader
- [x] **EVI-8A** Benchmark adapter v2 I
- [x] **SOA-2** Preflight, device resolution, run metadata, secret redaction
- [x] **SOA-3** Open-loop scheduler, HDR interval logs, self-monitoring, null server
- [x] **SOA-4** Deterministic workload model and body encoders
- [x] **S10** Write-once durable seal with id assigned at commit

## Phase 5 — Gate, serve path, input domain (was W3)

- [x] **EVI-10A** Library stored-range interpolation with input caps
- [x] **S11 / D-S11** Rollup into `segment_index.db`, folded in the seal and swap transactions
- [x] **SEC-3** Sealed route registry, frozen scope table, fail-closed gate
- [x] **SEC-4** One HTTP/1.1 serve path with pre-auth listener limits (absorbs ROB-23's caps)
- [x] **ROB-6** Bounded decimals
- [x] **EVI-8B** Benchmark adapter v2 II
- [x] **SOA-5** Soak ingest client
- [x] **SOA-12** Linux system samplers, contamination flags, strict-mode flush check

## Phase 6 — Auth, config, ILP fix (was W4)

- [x] **S12** Startup recovery, quarantine with TTL, fsck endpoint, `/ready` report
- [x] **S13 / D-S13** Idempotent atomic ingest entry point (library)
- [x] **SEC-5** rustls/ring TLS listener
- [x] **SEC-6** API keys, Bearer auth, keys file and keygen
- [x] **ROB-3** Panic containment, daemon supervision, Daemons registry, `weftdb::exec`
- [x] **PAC-5b** Strict configuration with a store filesystem check
- [x] **TAG-0** ILP series merging fixed
- [x] **ROB-8** Frame decoding hardened
- [x] **EVI-11** Stored WeftDB benchmark adapter
- [x] **SOA-6** Soak oracle core

## Phase 7 — Ingest wiring, whole-store backup, legacy seal (was W5)

- [x] **S14 / L-INGEST** HTTP ingest wiring
- [x] **S15 / D-S15** Partial sidecar v4
- [x] **S16 / D-S16** Whole-store backup with hard-linked frames
- [x] Whole-store backup manifest (7.3/7.4; Turso backup residue (d))
- [x] **S19 / D-S19** Seal-backed legacy aspects
- [x] **ROB-4** No-panic lint; weft-arrow `Result` API
- [x] **ROB-10** Admission I
- [x] **SEC-8** Startup ordering, exposure matrix, auth-off guard, metrics auth
- [x] **SEC-10** Exposure and secrets hygiene
- [x] **FRE-5** Exact-case compute vocabulary, no silent narrowing
- [x] **FRE-7b** Immutable declarations, scale rules, schema read-back
- [x] **EVI-13** DuckDB adapter I
- [x] **EVI-15R** Report renderer with generated honesty sections
- [x] **EVI-16** Honesty pages I
- [x] **SOA-7** Soak window digests with an oracle self-test
- [x] **SOA-8** Failure deck, recovery windows, SIGTERM verdict, maintenance driver

## Phase 8 — Durability CI, store split, scheduler (was W6)

- [x] **S20** Change log replaces the per-timestamp unbatched queue for seal-backed aspects
- [x] **S21 / D-S21** Crash-matrix CI, subprocess SIGKILL soak, durability benchmark and re-baseline, with **SOA-9** (`crash_kill9`)
- [x] **X-SPLIT-STORE** Split `segment_store.rs` into `open`/`read`/`seal`/`maint`/`stats`
- [x] → **TAG-4** weftdb read sites on the frame-reader API with typed errors
- [x] **FRE-6b** Strict request parsing and caller updates
- [x] **SEC-7** Key reload
- [x] **SEC-9** Audit
- [x] **SEC-11** Aspect-name traversal HTTP regression tests
- [x] **PAC-7** Operator CLI, exit-code contract and store identity at startup
- [x] → **ROB-11** Admission II
- [x] **ROB-19** Maintenance scheduler, compaction on by default, per-aspect circuit breaker
- [x] **SOA-10** Derived-read checks
- [x] **SOA-14** Server hooks
- [x] **EVI-14** DuckDB adapter II
- [x] **EVI-18** Backup health metrics and catch-up-free ticking
- [x] **EVI-25** Backup, restore and recovery cost measurement
- [x] **4.3** Quality pruning: point reads and downsample skip all-null segments and pages

## Phase 9 — Security and packaging close; Weft-Bench, SDKs and GPU flagship land (was W7)

- [x] **FRE-12b** Scope pin and `GET /api/v1/version`
- [x] **ROB-13** Bounded stored downsample and point lookups
- [x] **ROB-7** Ingest decode bounds and `400 not_finite`
- [x] **ROB-16** GPU configuration, compute limits, interpolation provenance and metrics
- [x] **ROB-23r** Probe-only admin listener
- [x] **ROB-5** Legacy queue-consumer panic fix
- [x] **SEC-13** Security acceptance
- [x] TLS on `weft-server`
- [x] API-key/token auth + basic RBAC + audit log
- [x] **PAC-13** Offline `weft-server restore` CLI
- [x] **PAC-9** Docker image and container smoke test
- [x] **PAC-11** systemd unit and host-install CI
- [x] **EVI-15** First cross-engine report and its owner-approved narrative
- [x] **EVI-19** Document RPO/RTO against the measured snapshot cadence (7.4)
- [x] **SOA-11** Soak backup lane with final restore verification
- [x] Weft-Bench: `weft-bench` and `weft-xbench`, fair protocol, reports, Grafana dashboard, pooled cross-engine run (#140–#150, #393, #505–#508)
- [x] DONE (2026-11-02) — External-engine adapter fairness — three concrete rules from the 2026 landscape (#393)
- [x] Add the **DuckDB** adapter
- [x] Add ClickHouse, InfluxDB 3, QuestDB, TimescaleDB adapters
- [x] Stored-read `.serialize` spans
- [x] Seal-path `control_plane.index.insert` span with `rows_changed`
- [x] Bench-run routes (`/bench/runs`)
- [x] **Python SDK** (2026-11-03)
- [x] **Rust SDK** (2026-11-04)
- [x] **Arrow Flight** (2026-11-04)
- [x] **Grafana** (2026-11-04)
- [x] **R/Arrow workflows** (2026-11-04)
- [x] Streamed stored interpolation `GET /api/v1/storage/{aspect}/interpolate` (#572, #585)
- [x] Startup hardware calibration for `Backend::Auto`
- [x] Float handling: shortest round-trip widening (#553) and `run_f64` range output (#575)
- [x] GPU benches: `--gpu-economics` (#569), conformance matrix (#573), `weft-e2e-interp` (#586)
- [x] Opt-in multi-GPU (#584) and mmap segment reads (#587)
- [x] `weft-bench --gpu-memory` device-memory soak (#583)
- [x] 5.1 command batching and 5.2 async handles measured and dropped
- [x] **B-interp** Interpolate-on-read, single-instant lookup, out-of-range extrapolation
- [x] Month 1 — Benchmark truth
- [x] Month 2 — Public API & observability
- [x] Months 4–5 — GPU flagship

## Phase 10 — Tags core (was W8)

- [x] open lane: **PAC-6** Library store options, generated configuration reference and registry scan
- [x] → **TAG-9** Frame error classes in recovery
- [x] → **TAG-5** Tags core: series table, catalog, and the layout marker
- [x] seal lane: **TAG-7** Tagged seal in the library
- [x] → **ROB-17** Disk guard, headroom API and space gauges
  - [x] Log the `507 insufficient_storage` refusal once per transition
  - [x] Report the guard's state on `/ready`
  - [x] ROB-18 phase 1: `DiskGuard::admit` with the reserved body size (#681)
- [x] maint lane: **TAG-6** Series-partitioned maintenance with multi-plan swaps
- [x] read lane: **TAG-8** Library series reads
- [x] **4.3** Tag pruning: single-series reads prune in SQL (`0007_series_time_index`, #158)
- [x] **4.6** Skip-identical reconciliation and upsert keys `timestamp + K` (`0008_upsert_keys`)
- [x] **FRE-8b** Stored-value text and one float-input rule across every ingest format
- [x] **FRE-9a** OpenAPI generated from the route table, committed, drift-checked in CI
- [x] **ROB-9** Mutation harness for frames, sidecars and Parquet
- [x] **SEC-14** Security documentation, config schema and upgrade notes
- [x] **PAC-10** Publish the multi-arch image (only after API auth)
- [x] **EVI-17** Honesty pages II
- [x] **FRE-13** Layout-1 to layout-2 upgrade test
- [x] **SOA-13** SoakReport, SLO evaluator R1–R8, coverage floors, honesty section
- [x] → **SOA-15** parts 1 and 3: nightly soak workflow and nightly-to-nightly comparison (#616)

## Phase 11 — Tag read surface, tag gate flip, compaction closes (was W9 shipped lanes and W10)

- [x] storage read handlers lane: **TAG-10** HTTP read surface
- [x] **PAC-14a** UPGRADING.md and compat legs A (upgrade) and C (rollback after writes) (absorbs FRE-19)
- [x] **SOA-16** Soak upgrade lane
- [x] → **TAG-11** HTTP ingest with tags and the tagged-write gate flip
- [x] → **B-tags** (TAG-5..TAG-11)
- [x] **ROB-21** Maintenance cancellation, RAII cleanup guards, minimal maintenance metrics
- [x] read lane: **ROB-15** Pagination contract freeze
- [x] **FRE-15** STABILITY.md, semver policy and the release-criteria wording
- [x] **PAC-15** Operator documentation
- [x] README `## Status` is stale (fixed 2026-11-10 by #682)
- [x] Two `weft-server` bit-exactness tests failed under `cargo test -p weft-server` (fixed with serde_json `float_roundtrip`)
  - [x] Measure what `float_roundtrip` costs the float parse itself
  - [x] Run `cargo test -p weft-server` standalone in CI
  - [x] Also test `weftdb`'s lib standalone there
- [x] The seven slow `weft-orchestration` tests are bounded; #402 closed on erasure
  - [x] The bound exists
  - [x] #402 is closed as completed
- [x] `weft-orchestration`: per-pipeline signal state (#673)
- [x] `weft-orchestration`: bounded-window threshold detectors (#692)
- [x] `weftdb` dictionaries: lossless `OccurrenceLog` (#612, #699)
- [x] `weftdb` memory per database file: idle close and heap trim (#699)
- [x] DONE (2026-11-10) — `weft-orchestration` is published

## Phase 12 — Close the pre-beta lanes (was W6, W8 and W9 open items)

- [ ] START HERE (filed 2026-11-11 for the next run)
- [ ] START HERE (rewritten 2026-11-11 for the next run)
- [ ] START HERE (filed 2026-10-11 for the next run)
- [ ] START HERE (filed 2026-10-09 for the next run)
- [ ] (2026-10-08 list.) The 2026-10-08 run (PR `routine/dsp-2026-10-08`)
- [ ] (2026-09-23 list; item 3 shipped 2026-10-08.) The 2026-09-23 run
- [ ] Run D-S21's privileged power-loss validation (sudo: dm-log-writes or a QEMU hard reset) *(owner)*
- [ ] read lane: **X-READ** Read-time newer-wins merge across overlapping segments, per series
- [ ] maint lane: **ROB-20** Maintenance resource limits, k-way merge, re-split of oversized frames
- [ ] open lane: **ROB-22** Suspect-frame policy and background-decode crash-loop breaker
- [ ] L-INGEST: **ROB-18** Two-phase ingest admission, total body deadline, panic mapping by phase
- [ ] **FRE-10a** Contract tests: probes, compute endpoints, metrics and roles
- [ ] **ROB-14** Cgroup-aware memory budget with per-request reservations
- [ ] Restoring *into a chosen new root* over HTTP (7.3)
- [ ] Store-side row-level TTL/expiry (prerequisite for the SOA-15 TTL lane)
- [x] Real-filesystem ENOSPC test for #656 on the injectable space source (#694 merged)
- [ ] **Measurement bulk ingest routed through the `.weftseg` seal**
- [ ] **CI**
- [ ] Packaging (1.0 minimum)
- [ ] Make the ghcr.io weftdb package public after PAC-10's first push *(owner)*
- [ ] Write or approve the honesty-page narrative *(owner)*
- [ ] Report the three `parquet` 60.0.0 decoder panics and the `arrow-buffer` 60.0.0 panic upstream to apache/arrow-rs *(owner: outward-facing)*
- [x] What `float_roundtrip` costs a whole JSON compute or ingest request
- [ ] Flaky under GPU contention: `gpu_memory::tests::the_pool_stays_bounded_and_stable_on_a_real_gpu`
- [x] Guard the `#[serial]`-above-`#[test]` ordering (`scripts/ci/check-serial-order.py`, #405; keyed `#[serial(key)]` covered 2026-11-13)
  - [ ] Contribute the minimal ordering repro upstream to rust-lang/rust#100263, still open (last updated 2024-12-10) *(owner: outward-facing)*
- [ ] Shared Cargo target dir aliases workspace crates across worktrees (found 2026-11-13): a `main` worktree built into the same `target-dir` left its `weft-reduce` artifact in place, and the branch's next `cargo build --release -p weft-server` linked it as fresh, so the binary ran `main`'s reduction under the branch's `weftdb` (caught by a gdb backtrace into the other checkout's source). Give each worktree its own `CARGO_TARGET_DIR`, or `touch` the workspace crates after switching

## Phase 13 — Contract candidate (was W9 gates and W11)

- [ ] **7.3 Corruption detection** (D-S12 + ROB-8/9/22 + HTTP restore)
- [ ] **7.5 Compaction** with query consistency, cancellation, and resource limits (ROB-19/20/21)
- [ ] **Compatibility tests** (FRE-13 + PAC-14a)
- [ ] Ship honesty pages / **Honesty pages shipped** (EVI-16, EVI-17 and the owner narrative)
- [ ] Move the T7 reference drive (`/mnt/weftbench`) to a 10 Gbps port before calibration *(owner)*
- [x] **FRE-9b** OpenAPI: storage and operator endpoints — the last change before the beta tag
  - [x] Extend the release tag grammar to `vX.Y.Z-beta.N` (`TAG_RE` in `scripts/release/verify.sh`)
- [x] **FRE-10b** Contract tests: storage and operator endpoints, plus coverage
- [ ] **TAG-12** Multi-series crash matrix, fan-out and TSBS benchmarks, fixtures, freeze docs
- [ ] **ROB-24** Limits and overload evidence; frozen configuration documentation
- [ ] **SOA-15** part 2: TTL lane

## Phase 14 — Beta tag (was W11)

- [ ] Approve and push the **v1.0.0-beta.1** tag on FRE-9b's merge *(owner)*
- [x] **FRE-16** CI compatibility gates: OpenAPI breaking-change check, fixture guard, golden coverage
- [ ] **7.6 Quotas/limits** (ROB-14 + ROB-24 evidence)
- [ ] **Panic audit on server paths** (ROB-2/3/4 + ROB-24)

## Phase 15 — Calibration (was W11)

- [ ] **SOA-17** Calibration on a pre-grown store, frozen acceptance profiles, `scripts/crash-soak.sh` — on the beta tag

## Phase 16 — Durability acceptance and legal review (was W12; W13 counsel)

- [ ] **Phase 7 acceptance run** (24 h, published as a benchmark artifact)
  - [ ] **SOA-18** 24 h acceptance run on the reference SSD, published as an artifact
  - [ ] Run the privileged power-loss validation on the acceptance commit *(owner)*
- [ ] Legal review of competitor benchmark-publication terms + codec licenses
  - [ ] Review each competitor's benchmark-publication terms (kdb+ especially)
  - [ ] **Open with counsel:** Amazon FOR (US 11,308,093), Tiger Data, SAP (before 6.2), **WEFTDB** trademark clearance, the benchmark policy
  - [ ] **Codec patent/licence gate (owner decisions 2026-10-08).**
    - [ ] **Default FOR codec and the Tiger Data family: with counsel.**
    - [ ] **The 2026-10-08 routine's advisory codecs: not yet reviewed, and ungated.**
    - [ ] **SAP family: review before the 6.2 design.**
    - [ ] **Package licence metadata: mitigated by the relicense; the expression is with counsel.**
  - [ ] **With counsel** (Licensing/legal): Amazon US 11,308,093 against the default FOR codec, the Tiger Data family, the SAP family, **WEFTDB** trademark clearance, the benchmark policy
  - [ ] Settle the counsel items (codec patents, **WEFTDB** trademark, benchmark policy) before rc.1 *(owner)*

## Phase 17 — Freeze (was W13)

- [ ] **7.2 WAL & crash consistency** (crash matrix, kill test and power-loss run green)
- [ ] **FRE-18** rc.1 freeze cut and layout-2 baseline fixtures; the OpenAPI breaking-change check blocks

## Phase 18 — Release candidate (was W13)

- [ ] Approve and push the **v1.0.0-rc.1** tag on FRE-18's merge *(owner)*
- [ ] **Public surface declared and frozen**

## Phase 19 — RC verification on release artifacts (was W14)

- [ ] **SOA-19** RC soak on the rc release artifact in the shipped default config (8 h, or 24 h)
- [ ] **EVI-21** 1.0 evidence run at the rc: strict claims, cross-engine matrix re-run
- [ ] **PAC-18** Dress rehearsal: compatibility matrix with the N-1 leg, container and systemd jobs on the rc

## Phase 20 — 1.0 claims and release notes (was W14 and W15)

- [ ] Every performance claim in the README links to a benchmark artifact
- [ ] Upgrade/rollback procedure, exercised against the previous release
- [ ] Write or approve the 1.0 release notes and upgrade notes before the cut *(owner)*
- [ ] **EVI-22** README 1.0 project status, badge and versions

## Phase 21 — 1.0.0 (was W15)

- [ ] README *Project status* table updated to reflect 1.0 and the status badge changed from `pre-beta`
- [ ] **PAC-18** Cut 1.0.0: binaries, image, release notes
  - [ ] Approve and push the **v1.0.0** tag *(owner)*
- [ ] **Semver policy + `CHANGELOG.md`**, starting from the first tagged pre-release

## Phase 22 — Connectors and ingest interop

- [ ] **B-conn** Vendor-neutral connector trait + registry
- [ ] **B-ilp** ILP/Influx interop connector crate
- [ ] **B-rest** REST facade remainder
- [ ] **B-resilience** Write retry, size-tiered write selector, interpolation window extension
- [ ] **B-conncache** Complete the cached-connection migration
- [ ] Grafana → Prometheus remote write/read (if monitoring) → R/Arrow workflows
  - [ ] **Prometheus remote write/read**: a remote-write series is a metric plus a label set
- [ ] Lean on production MVCC concurrent writes for concurrent ingest + catalog updates under load
- [ ] **S17** Relaxed durability mode with synced_epoch watermark *(optional for 1.0)*

## Phase 23 — Online ingest

- [ ] **7.1 Online ingest**
  - [ ] **B-poll / B-retry / B-register** Scheduled polling daemon, at-least-once retry buffer, runtime source registration
  - [ ] **B-dedup** Event-UUID idempotency ledger + delete-after-success queue

## Phase 24 — Commercial hardening and paid beta

- [ ] Security
- [ ] Compliance readiness
- [ ] Packaging
- [ ] Observability
  - [ ] Add end-to-end timing spans (per-pipeline-stage spans)
- [ ] SDKs — Rust → Python → TypeScript → R/Arrow
- [ ] Licensing/legal
  - [x] **Contribution terms: DCO or CLA, the contributor's choice.**
    - [ ] **Enforce it (owner, once on main):** `cla-signatures` branch, `cla-v1` tag and its ruleset
  - [ ] Still open: open-core vs commercial split; customer-data handling
- [ ] Buyer calls that test the willingness-to-pay hypotheses
- [ ] Encryption at rest (AEAD pager) for catalog/metadata/pipeline-state DBs
- [ ] **Backup 7.4 residue (b) — incremental / WAL-streaming backup** beyond the full `VACUUM INTO`
- [ ] **Backup 7.4 residue (b) is UPSTREAM-GATED, not an open design question (research 2026-09-23)**
- [ ] **Backup cost residue (a) — the root cause is documented upstream, and it is per-call fsync + TRUNCATE checkpoint (research 2026-09-23).**
- [ ] **Backup cost residue (b) — (i) MEASURED 2026-10-08: `synchronous=OFF` is NOT a material win.**
- [ ] **Backup cost residue (c) — DO NOT reach for Turso's experimental passive checkpoint.**
- [ ] **Backup cost residue (d) — evaluate an `sqlite3_rsync`-style page-delta replica.**
- [ ] Triggers (`BEFORE/AFTER/INSTEAD OF` + `WHEN`)
- [ ] Dynamic auth tokens as closures
- [ ] `UPDATE … FROM`, aggregate `FILTER`, `INDEXED BY`, `NULLS FIRST/LAST`
- [ ] Month 6 — Commercial beta

## Phase 25 — Reduction, sketch, codec and benchmark research

- [ ] **ALP now has a FROZEN wire layout to mirror — do not invent a container (research 2026-09-23).**
- [ ] **Set the ALP acceptance bar from upstream's own ablation, and be willing to DECLINE.**
- [ ] **GPU-decode ALP — retarget to G-ALP (DaMoN'25), and decide the exception layout BEFORE the CPU ALP block is frozen on disk.**
- [ ] **NEXT — UDDSketch (uniform guarantee under collapse):**
  - [x] The sketch itself, as a library type (2026-11-11)
  - [x] A bench for the cost against `DdSketch` (2026-11-11)
  - [x] The accuracy each collapse costs on real data (2026-11-11)
  - [x] Repeat the timing figures on a quiet host with pinned cores before citing any of them
  - [ ] **Research (2026-11-11) — TimescaleDB's default percentile aggregate is a UDDSketch, so it is the like-for-like comparison.**
  - [ ] Decide whether the `sketch_p*` reductions take it (owner-gated; needs a sidecar version step)
- [ ] **Research (2026-10-11) — the exact TWA is now a measurable differentiator.**
- [ ] **POSITIONING ALERT — WeftDB's precision wedge now has a direct competitor (research 2026-09-23).**
  - [ ] **NEXT — the remaining 6.3×:**
    - [x] **DONE (2026-10-09) — the `avg` division was the biggest piece of `reduce_scaled`**
  - [ ] **NEXT — the sidecar build.**
  - [ ] *(original item)* **a `ScaledI64`-native reduction fast path in `weft-reduce`**
- [ ] **BENCHMARK HYGIENE — the synthetic ingest corpus flatters WeftDB, by a lot (measured 2026-07-23).**
  - [ ] Audit the other `weft-bench` workloads' generators for the same class of flattery
    - [ ] **NEXT — a representative storage generator, or real-corpus defaults.**
    - [ ] **FOUND (2026-10-09) — `sketch_p99`'s published ~2.6× speed-up over exact `p99` was the generator's.**
      - [ ] **Research (2026-10-09) — a cheaper sketch index, if the sketch format is ever revised.**
      - [ ] **Positioning (2026-10-09) — exact percentiles are a differentiator against QuestDB.**
      - [x] **NEXT — select on the mantissas when a partial is not merged.** (#401)
        - [x] TWA on the finish-only route (`twa`, `twa_linear`, `twa_bucket_end` weighted on the mantissas)
        - [ ] Several segments of one scale: carry mantissas, not `BigDecimal` samples, in the in-memory partial so a multi-segment exact percentile selects on integers too (the `.weftpart` sidecar never holds exact-percentile samples, so no format step)
- [ ] **Adopt the TSM-Bench QUERY SPEC (not the repo) as Weft-Bench's external comparison workload.**
- [ ] **Research (2026-10-09) — DuckDB has a native linear-interpolation arm**
- [ ] **NEXT — (3c) materialized tiers, remaining**
- [ ] **Positioning — WeftDB's `.weftpart` sidecar IS the "incremental materialized view", and mergeable DDSketch is its edge (this run's research)**
- [ ] **NEXT — materialize the sidecar at a coarser DEFAULT + adopt sketches**
- [ ] **NEXT — default target for the compaction daemon**

## Phase 26 — Compression v2: lossless typed codecs

- [ ] **6.1 Lossless typed codecs**
  - [ ] **Possible future: a patent-clear codec in a new crate.**
  - [ ] **Evaluate Pcodec as an integer/f64 codec + bench baseline — now with the upstream numbers (research 2026-07-23)**
    - [ ] **SCOPE CORRECTION (research 2026-09-23): evaluate pco for the COLD TIER and as a bytes/point ceiling — not as the hot-path value codec.**
    - [ ] **The real prize in pco may be its MODE DETECTION, not its format.**
    - [x] **DONE (2026-10-08, advisory) — factor a timestamp column's common multiple (pco's `IntMult`) before DoD packing.**
      - [ ] **NEXT — realize the common-multiple factor as a timestamp codec flag (owner sign-off: headline bytes/point change).**
  - [ ] **f64-codec adopt: target ALP, not Chimp128/Elf — the state of the art.**
  - [x] **DONE (2026-10-08) — Use the `alp` crate to get the ALP adopt benchmarked, rather than hand-rolling it first.**
    - [ ] **ALP adopt as `VAL_CODEC_ALP` — owner sign-off (headline change).**
    - [ ] **NEXT — realize the decimal-exponent FOR as `VAL_CODEC_DFOR` (owner sign-off: headline bytes/point change).**
    - [ ] **Re-decide the transposed layout now that the linear baseline is honest — RECOMMEND RETIRING `VAL_CODEC_TRANSPOSED` to read-only (owner decision).**
    - [ ] **Next read-path floor: every point read still CRCs the whole frame**
  - [ ] **Evaluate Vortex as a columnar interchange + benchmark reference** (also filed as "Next slice — evaluate Vortex as interchange + bench baseline")
    - [ ] **GO on both counts, and the governance question is settled (research 2026-09-23)**
    - [ ] **Do NOT take a build dependency on `vortex-gpu`.**
- [ ] **NEXT — upstream FastLanes does NOT make the transposed layout its default either, and exposes the permutation instead (research 2026-09-23).**
- [x] **DONE (2026-10-08) — Cross-check WeftDB's hand-rolled bit-plane decoder against the `fastlanes` crate (0.7.2, 2026-09-02, Apache-2.0).**
  - [ ] **Residue — still 2.4–7× behind `fastlanes` (0.33 ms flat across widths).**
- [ ] **Track the FastLanes SPEC, not the CWI reference implementation.**
- [ ] **Evaluate the FastLanes *File Format* (not just the layout) as a bench baseline + a random-access reference**
  - [ ] **It publishes a random-access target WeftDB can mirror exactly (research 2026-09-23)**
  - [ ] **Two honesty corrections from the same paper.**
- [ ] **Point-lookup positioning — WeftDB's block-random-access read is a genuine columnar mitigation**
- [ ] **Cascade adopt-or-drop into the DEFAULT selector — owner-gated (headline change, as FOR/ALP)**
- [ ] **NEXT — checkpoint index adopt-or-drop into the DEFAULT policy — owner-gated (bytes/point change, as FOR/cascade/ALP)**
- [ ] **NEXT — a random-access-capable Gorilla/RLE would widen the checkpoint index's reach**
- [ ] **Correlation-aware column compression as a post-B-tags codec direction**
- [ ] **B-precision** `BigDecimal` math audit; improved `simplify`
- [ ] **B-msgpack** MessagePack for queued artifacts
- [ ] Month 5 — Compression v2 prototype

## Phase 27 — Compression v2: model-based and compressed-domain execution

- [ ] **6.2 Model-based compression** (leverages WeftDB's spline DNA, NeaTS-like)
- [ ] **6.3 Late/compressed-domain execution**
- [ ] **6.4 Quality benchmarks**
- [ ] **Fuse decompression INTO the GPU interpolation kernel — with a number behind the wedge.**
- [ ] **GPU-decode the `.weftseg` transposed layout rather than adopting a new format**

## Phase 28 — Analytics premium

- [ ] Sliding windows → overlapping windows → event-centered windows
- [ ] Resample-before-compare
- [ ] Pattern dedup → occurrence-distance constraints
- [ ] Correlation / signal benchmarking
- [ ] Per-stage audit trail
- [ ] **B-windows** Sliding/overlapping + event-centered windows, horizon fan-out, min-density validation
- [ ] **B-sim** Full variability-metric matrix, fixed-point dedup, occurrence-distance constraint
- [ ] **B-analysis** Verify the per-point analysis model is wired end-to-end
- [ ] **B-snapshot** Pipeline state-snapshot audit trail
- [ ] **B-object** Schemaless object/annotation store beside numeric aspects

## Phase 29 — SQL surface

- [ ] SQL surface (DataFusion)

## Phase 30 — Flight SQL, ML/AI and evaluations

- [ ] Python SDK → Rust SDK → Arrow Flight / Flight SQL → SQL surface / DataFusion
  - [ ] **Flight SQL**
- [ ] *(Future)* ML/AI
- [ ] Evaluate (don't rush): vector search over pattern/shape summaries; CDC/sync engine; custom I/O
