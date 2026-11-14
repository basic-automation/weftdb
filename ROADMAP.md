# WeftDB Roadmap

One checklist, in phases. Each phase starts only when the phase before it is done, and nothing in a phase depends on an item in a later one. Item IDs (`7.2`, `6.1`, `S17`, `FRE-9b`, …) are stable and match DECISIONS.md, issues and PRs. Prose, measurements, audit notes and research notes are in [docs/roadmap-notes.md](docs/roadmap-notes.md).

Phases 1–11 are complete and are summarized in prose; the item IDs they name still match DECISIONS.md, issues and PRs. In the open phases, sub-items run top to bottom under their parent.

Phases 1–11 record shipped work. Phases 12–21 lead to 1.0.0. Phases 22–30 follow 1.0.

## Phase 1 — Benchmark thesis and foundations

**Complete.** Phase 1 set out the benchmark thesis and built the foundations everything else stands on. The design-partner stand-in datasets, willingness-to-pay hypotheses and a methodology document were published before any performance claim. The first `axum` server shipped `POST /api/v1/interpolate` over `splimes` 1.0, ILP and batch ingest, downsample and aggregation, point, stored-range and single-instant reads, maintenance and an HTTP catalog. It took Parquet and CSV in, put Arrow and Parquet out, paged stored-range reads by cursor, and exported Prometheus histograms, OpenTelemetry traces and `/metrics`.

The control plane settled on Turso (0.4 → 0.6 → 0.8.1, with plain `INTEGER PRIMARY KEY` for MVCC). It gained online `VACUUM INTO` backups with a concurrent-write-safe verifier and a background daemon, restore and a drill exposed at the API, and exact `n_change()` row accounting. The storage groundwork landed as well: the UTC time policy (**4.1**), preserved legacy MVCC logs (**S1**), a durable I/O layer with fault points and a power-cut simulator (**S2**), and store-wide sweep isolation (**S4**). On the read and reduction side it delivered the integer-native downsample path, streaming and batch point lookups, windowed range reads, `weft-reduce` (mergeable DDSketch, cross-segment downsample, size-targeted compaction), per-segment partials at seal time, and the first Compression v2 codecs realised on disk (Gorilla+RLE, bit-packing, scaled-integer, FOR, an f64 best-of selector, cascades, FastLanes layout). Test infrastructure was made reliable: GPU flakes, a rustc ICE and link OOMs were root-caused, and `cargo test --workspace` completes. The one unfinished line, Months 3–4 hot-path optimization, moved to Phase 25.

## Phase 2 — Lock the store shapes (was W0)

**Complete.** Phase 2 (W0) locked the store shapes later work depends on. The splimes-1, legal and durability branches landed on main (**INT-0**), and storage v2 gained hardened open (**S3**), atomic backup publish with a manifest, retention and drill hygiene plus the backup-label grammar (**S5**, **SEC-1**), schema v2 with `IndexTxn`, write poison and root-relative frame paths (**S6**), a persistent per-aspect id allocator with commit and maintenance locks (**S7**), and legacy rows-mode hygiene (**S18**). Decision 1 recorded the shapes S6 and S7 rely on.

The phase also stood up the release pipeline (**PAC-1a**), the canonical tag types (`TagSet`, `SeriesKey`, `SeriesSelector`) with frozen caps (**TAG-1**), and the security process, including private vulnerability reporting, which is now on.

## Phase 3 — Baseline release, server split, goldens (was W1)

**Complete.** Phase 3 (W1) cut the v0.1.0 baseline (**PAC-4**, a GitHub release) and froze what it wrote: golden frames, sidecars and persisted encodings (**FRE-3**) and a fixture set written by the v0.1.0 binary (**X-FX**). Crates got per-crate versions, a single edition, a publish split and package hygiene (**FRE-1**); `ValueKind` became open with a provenance scope (**TAG-3**); and the server was split into `manage.rs` and `main.rs` (**X-SPLIT-SRV**) with a configuration-registry skeleton (**PAC-5a**).

Write-once maintenance landed in two parts (**S8**, **S9**). On the evidence side, README commands and links were checked (**EVI-1**), bench runs got a manifest (**EVI-3**) and `--sparse`/`--scale` workloads (**EVI-9**), and the soak harness got its skeleton (**SOA-1**).

## Phase 4 — Error contract, frame format, portable builds (was W2)

**Complete.** Phase 4 (W2) fixed the external contracts. Every error goes through one envelope and code table (**X-ERR**); aspect names are encoded and validated at declare (**X-NAME**); a storage-engine panic fails stop (**ROB-2**); and frames have a registry with the v7 envelope (**X-FRAME**). The durable write-once seal assigns segment ids at commit (**S10**). Builds became portable: a stripped dist profile with a third-party licence bundle (**PAC-1b**), library crates that impose no global allocator or wgpu features (**PAC-3**), the crate publishing split, Linux binaries on a glibc 2.28 baseline (**PAC-2**), and `weft-server import-rows-mode` for 0.1.x data (**FRE-14c**).

Benchmark evidence became automated: a nightly regression workflow with `weft-bench compare` (**EVI-5**), `bench/claims.json` checked in CI (**EVI-2**), the artifact pipeline (**EVI-4**), an ingest workload over `SegmentStore` with the UCI loader (**EVI-6**) and benchmark adapter v2 part I (**EVI-8A**). The soak harness gained preflight, device resolution, run metadata and secret redaction (**SOA-2**), an open-loop scheduler with HDR interval logs (**SOA-3**), and a deterministic workload model (**SOA-4**).

## Phase 5 — Gate, serve path, input domain (was W3)

**Complete.** Phase 5 (W3) put the gate and the serve path in place. Routes live in a sealed registry with a frozen scope table and a fail-closed gate (**SEC-3**), and every request goes through one HTTP/1.1 serve path with pre-auth listener limits (**SEC-4**, absorbing ROB-23's caps). Rollups moved into `segment_index.db`, folded into the seal and swap transactions (**S11**); decimals are bounded (**ROB-6**); and the library gained stored-range interpolation with input caps (**EVI-10A**).

Benchmark adapter v2 was finished (**EVI-8B**), and the soak harness gained its ingest client (**SOA-5**) and Linux system samplers with contamination flags and a strict-mode flush check (**SOA-12**).

## Phase 6 — Auth, config, ILP fix (was W4)

**Complete.** Phase 6 (W4) added auth, strict configuration and recovery. The server gained a rustls/ring TLS listener (**SEC-5**) and API keys with Bearer auth, a keys file and `keygen` (**SEC-6**); configuration became strict with a store filesystem check (**PAC-5b**). Startup recovery, quarantine with a TTL, the fsck endpoint and the `/ready` report shipped (**S12**), along with an idempotent atomic ingest entry point in the library (**S13**).

Robustness work contained panics, supervised daemons through a `Daemons` registry and added `weftdb::exec` (**ROB-3**), and hardened frame decoding (**ROB-8**). ILP series merging was fixed (**TAG-0**), the stored WeftDB benchmark adapter landed (**EVI-11**), and the soak oracle core was built (**SOA-6**).

## Phase 7 — Ingest wiring, whole-store backup, legacy seal (was W5)

**Complete.** Phase 7 (W5) wired HTTP ingest into the segment store (**S14**) and made backups whole-store: hard-linked frames (**S16**) with a manifest covering items 7.3/7.4 and the Turso backup residue. Partial sidecars moved to v4 (**S15**), and legacy aspects became seal-backed (**S19**). Ingest gained admission control (**ROB-10**), a no-panic lint with a `Result` API in `weft-arrow` (**ROB-4**), an exact-case compute vocabulary with no silent narrowing (**FRE-5**), and immutable declarations with scale rules and schema read-back (**FRE-7b**).

On security, startup ordering, the exposure matrix, the auth-off guard and metrics auth landed (**SEC-8**), along with exposure and secrets hygiene (**SEC-10**). Evidence work added DuckDB adapter part I (**EVI-13**), a report renderer with generated honesty sections (**EVI-15R**) and honesty pages part I (**EVI-16**). The soak harness gained window digests with an oracle self-test (**SOA-7**) and a failure deck with recovery windows, a SIGTERM verdict and a maintenance driver (**SOA-8**).

## Phase 8 — Durability CI, store split, scheduler (was W6)

**Complete.** Phase 8 (W6) made durability testable and split the store. A change log replaced the per-timestamp unbatched queue for seal-backed aspects (**S20**). Crash-matrix CI, a subprocess SIGKILL soak (**SOA-9**, `crash_kill9`), a durability benchmark and a re-baseline landed together (**S21**). `segment_store.rs` was split into `open`/`read`/`seal`/`maint`/`stats` (**X-SPLIT-STORE**), after which weftdb's read sites moved to the frame-reader API with typed errors (**TAG-4**). The maintenance scheduler shipped with compaction on by default and a per-aspect circuit breaker (**ROB-19**), followed by admission part II (**ROB-11**). Point reads and downsample skip all-null segments and pages (**4.3** quality pruning).

The server gained strict request parsing (**FRE-6b**), key reload (**SEC-7**), an audit log (**SEC-9**), aspect-name traversal regression tests (**SEC-11**), and an operator CLI with an exit-code contract and store identity at startup (**PAC-7**). The soak harness added derived-read checks (**SOA-10**) and server hooks (**SOA-14**). Evidence work added DuckDB adapter part II (**EVI-14**), backup health metrics with catch-up-free ticking (**EVI-18**), and measured backup, restore and recovery costs (**EVI-25**).

## Phase 9 — Security and packaging close; Weft-Bench, SDKs and GPU flagship land (was W7)

**Complete.** Phase 9 (W7) closed security and packaging and landed the benchmark suite, the SDKs and the GPU flagship. On security and limits: the scope pin and `GET /api/v1/version` (**FRE-12b**); bounded stored downsample and point lookups (**ROB-13**); ingest decode bounds with `400 not_finite` (**ROB-7**); GPU configuration, compute limits, interpolation provenance and metrics (**ROB-16**); a probe-only admin listener (**ROB-23r**); the legacy queue-consumer panic fix (**ROB-5**); and security acceptance (**SEC-13**), covering TLS, API keys with basic RBAC, and the audit log. Packaging shipped the offline `weft-server restore` CLI (**PAC-13**), a Docker image with a container smoke test (**PAC-9**), and a systemd unit with host-install CI (**PAC-11**). RPO/RTO is documented against the measured snapshot cadence (**EVI-19**), and the soak harness gained a backup lane with final restore verification (**SOA-11**).

Weft-Bench shipped as `weft-bench` and `weft-xbench`, with a fair protocol and adapter-fairness rules (#393), reports, a Grafana dashboard and a pooled cross-engine run against DuckDB, ClickHouse, InfluxDB 3, QuestDB and TimescaleDB (#140–#150, #505–#508). The first cross-engine report and its approved narrative followed (**EVI-15**). Tracing gained stored-read `.serialize` spans and a seal-path index span with `rows_changed`, and the server gained `/bench/runs`. The Python and Rust SDKs, Arrow Flight, Grafana integration and R/Arrow workflows shipped between 2026-11-03 and 11-04. The GPU flagship delivered streamed stored interpolation (#572, #585), interpolate-on-read with single-instant lookup and extrapolation (**B-interp**), startup hardware calibration for `Backend::Auto`, shortest round-trip float widening (#553, #575), the GPU economics, conformance and end-to-end benches (#569, #573, #586), opt-in multi-GPU (#584), mmap segment reads (#587) and a device-memory soak (#583). Command batching (5.1) and async handles (5.2) were measured and dropped. With this phase, the Month 1, Month 2 and Months 4–5 milestones closed.

## Phase 10 — Tags core (was W8)

**Complete.** Phase 10 (W8) built the tags core. The library gained store options, a generated configuration reference and a registry scan (**PAC-6**). Recovery learned the frame error classes (**TAG-9**). The series table, catalog and layout marker landed (**TAG-5**), followed by the tagged seal (**TAG-7**), series-partitioned maintenance with multi-plan swaps (**TAG-6**) and library series reads (**TAG-8**). Single-series reads prune in SQL (**4.3** tag pruning, #158), reconciliation skips identical rows, and upserts key on `timestamp + K` (**4.6**). A disk guard with a headroom API and space gauges shipped (**ROB-17**): it logs the `507 insufficient_storage` refusal once per transition, reports on `/ready`, and admits ingest bodies against their reserved size (ROB-18 phase 1, #681).

Stored-value text and one float-input rule now cover every ingest format (**FRE-8b**). OpenAPI is generated from the route table, committed and drift-checked in CI (**FRE-9a**). A mutation harness covers frames, sidecars and Parquet (**ROB-9**). Security documentation, the config schema and upgrade notes were written (**SEC-14**), and the multi-arch image was published after API auth (**PAC-10**). The phase also added honesty pages part II (**EVI-17**), the layout-1 to layout-2 upgrade test (**FRE-13**), the SoakReport with the R1–R8 SLO evaluator, coverage floors and an honesty section (**SOA-13**), and the nightly soak workflow with nightly-to-nightly comparison (**SOA-15** parts 1 and 3, #616).

## Phase 11 — Tag read surface, tag gate flip, compaction closes (was W9 shipped lanes and W10)

**Complete.** Phase 11 (W9 and W10) put tags on the HTTP surface and closed compaction. The HTTP read surface for tags shipped (**TAG-10**), followed by tagged HTTP ingest and the tagged-write gate flip (**TAG-11**), which completed **B-tags** (TAG-5 through TAG-11). Upgrades got UPGRADING.md and compat legs A (upgrade) and C (rollback after writes) (**PAC-14a**, absorbing FRE-19), plus a soak upgrade lane (**SOA-16**). Maintenance gained cancellation, RAII cleanup guards and minimal metrics (**ROB-21**). The pagination contract was frozen (**ROB-15**), STABILITY.md set the semver policy and release-criteria wording (**FRE-15**), and operator documentation was written (**PAC-15**).

The phase also cleared the remaining loose ends. The stale README status was fixed (#682). Two bit-exactness test failures were fixed with serde_json `float_roundtrip`, its parse cost was measured, and `weft-server` and `weftdb` now run standalone in CI. The seven slow `weft-orchestration` tests were bounded and #402 closed. `weft-orchestration` gained per-pipeline signal state (#673) and bounded-window threshold detectors (#692), and was published (2026-11-10). `weftdb` gained a lossless `OccurrenceLog` for dictionaries (#612, #699), and idle close with heap trim for memory per database file (#699).

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
- [x] Restoring *into a chosen new root* over HTTP (7.3) (#213, 2026-11-14: `POST /v1/storage/backups/restore?label=...&name=...` into `WEFT_RESTORE_BASE_DIR`; the offline drill at `weft-server/src/restore.rs` is unchanged)
- [ ] Store-side row-level TTL/expiry (prerequisite for the SOA-15 TTL lane)
- [ ] Real-filesystem ENOSPC test for #656 on the injectable space source (#694 merged)
- [ ] **Measurement bulk ingest routed through the `.weftseg` seal**
- [ ] **CI**
- [ ] Packaging (1.0 minimum)
- [ ] Make the ghcr.io weftdb package public after PAC-10's first push *(owner)*
- [ ] Write or approve the honesty-page narrative *(owner)*
- [ ] Report the three `parquet` 60.0.0 decoder panics and the `arrow-buffer` 60.0.0 panic upstream to apache/arrow-rs *(owner: outward-facing)*
- [ ] What `float_roundtrip` costs a whole JSON compute or ingest request
- [ ] Flaky under GPU contention: `gpu_memory::tests::the_pool_stays_bounded_and_stable_on_a_real_gpu`
- [x] Guard the `#[serial]`-above-`#[test]` ordering (#726, 2026-11-13: the guard reads the keyed `#[serial(key)]` form too and requires a test attribute in the same attribute block; either position in that block serialises, so the "above" ordering is no longer enforced)

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
- [x] **ROB-24** Limits and overload evidence; frozen configuration documentation
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

- [ ] Months 3–4 — Hot-path optimization: batch-ingest rewrite, minimized `BigDecimal` hot-loop use, typed-array interpolation streaming; establish whether storage or conversion is the dominant bottleneck (moved from Phase 1, 2026-11-13)
- [ ] **ALP now has a FROZEN wire layout to mirror — do not invent a container (research 2026-09-23).**
- [ ] **Set the ALP acceptance bar from upstream's own ablation, and be willing to DECLINE.**
- [ ] **GPU-decode ALP — retarget to G-ALP (DaMoN'25), and decide the exception layout BEFORE the CPU ALP block is frozen on disk.**
- [ ] **NEXT — UDDSketch (uniform guarantee under collapse):**
  - [x] The sketch itself, as a library type (2026-11-11)
  - [x] A bench for the cost against `DdSketch` (2026-11-11)
  - [x] The accuracy each collapse costs on real data (2026-11-11)
  - [ ] Repeat the timing figures on a quiet host with pinned cores before citing any of them
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
      - [ ] **NEXT — select on the mantissas when a partial is not merged.** (#401)
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
