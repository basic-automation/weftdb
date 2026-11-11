# WeftDB Roadmap

## Honesty pages

- [ ] Ship honesty pages

## Phased roadmap (0–9)

### Phase 0 — Benchmark thesis & buyer wedge · *Immediate*

Phase 0 produced a written record but no market evidence. Five open datasets with interpolation-heavy workloads stand in for design partners until Basic Automation is incorporated. They are listed in [DESIGN-PARTNERS.md](DESIGN-PARTNERS.md), and no partner has been contacted. The willingness-to-pay hypotheses are in [WILLINGNESS-TO-PAY.md](WILLINGNESS-TO-PAY.md). Every one of them stays untested until buyer calls happen.

### Phase 1 — Build `Weft-Bench` · *Highest priority*

Every Phase 1 line was in place by 2026-11-02 (DECISIONS 2026-11-02). Two tools do the measuring. `weft-bench` runs WeftDB's own workloads: gap fill, bulk-ingest growth, online ingest plus query, compressed query, the analytics pipeline, post-compaction reads (`--compressed-query-compact`), failure tests (`--failure kill-ingest|kill-compaction|corrupt-segment`), saturation sweeps and a seeded query mix. `weft-xbench` measures DuckDB, ClickHouse, TimescaleDB, QuestDB, InfluxDB 3 and IoTDB, plus TSBS `cpu-only` data through `--tsbs`. Where an engine supports it, it is measured in three interpolation classes: native fill, portable SQL and client-side. A native fill that computes a different answer is gated on shape and never raced. VictoriaMetrics was not built, because the plan conditions it on monitoring, and kdb+ is excluded by its licence. TSBS's tag-grouped queries moved to TAG-12. The fair protocol covers:

- raw `samples_ns` (at least 10 per short test)
- answer digests (`result_sha256`, `answer_sha256`)
- pooled runs through `combine`, which runs first and reports `between_run`
- OS-cold and post-restart runs (`--cold-hook`)
- a run manifest with binary and container image digests

Reports also come out as Markdown, CSV and Prometheus text, and there is a Grafana dashboard under `bench/grafana/`. `scripts/bench/check-separation.py` enforces anti-Goodhart separation in CI.

The key measurements:

- **Ingest:** 64k-row batches hold 3.9M → 3.2M rows/s up to 1B rows. 1,024-row batches fall from 141k to 107k rows/s by 100M.
- **Compressed query:** in-store downsample is 2.2–2.9× faster than decode-then-reduce over a year window.
- **Spline fill (NOAA NDBC buoy data):** cubic beats linear on 7 of 9 series at a two-hour outage and loses on 8 of 9 at a day. It does not beat linear on real prices.
- **Fixes the benchmarks drove:**
  - Analytics `prepare_data` dropped from ~420 s to 204 s (#505).
  - Squash compaction dropped from 694 s to 31–43 s (#507), with checkpointing and 4,096-row pages bringing small-window reads back to 0.18 → 0.25 ms.
  - Open-time recovery caught all 143 byte flips of a probe frame.
- **Pooled cross-engine run** (`bench/reports/phase1-pooled/`, #508): 319 of 320 gates pass. 105 of 112 verdicts hold across both runs and none flips. That gives 17 wins (all against portable SQL), 85 losses (every load, the stored downsample and every comparison with client-side interpolation) and 3 within noise.

Details are in #140–#150, #393 and #505–#508.

### Phase 2 — Benchmark-grade server/API · *Very high*

The `axum` server is in place with the flagship `POST /api/v1/interpolate` over the `splimes` engine, every point marked raw, interpolated or extrapolated. InfluxDB Line Protocol ingest, downsample/aggregation and single-instant point queries work, and so do stored-range and single-instant reads, storage maintenance, and HTTP catalog management with batch ingest.

Interchange and output are covered too: Parquet and CSV in, columnar Arrow/Parquet out of the compute endpoints, and cursor paging on the stored-range reads. Observability is wired in at the API, with Prometheus latency histograms and OpenTelemetry trace export alongside `/metrics`.

- [ ] Python SDK → Rust SDK → Arrow Flight / Flight SQL → SQL surface / DataFusion (later)
  - [x] **Python SDK** (2026-11-03): `clients/python` (package `weftdb`), standard-library only with `pyarrow` as the optional `arrow` extra. It covers every route: compute, catalog, the four ingest formats with `Idempotency-Key`, stored reads with cursor-following iterators, maintenance, backup and the bench runs. Storage values travel as `Decimal`/text and a `float` is refused at that boundary. The unit tests run against an in-process fake; `tests/test_live.py` is the contract test against a real `weft-server`, which CI runs on Linux after the build.
  - [x] **Rust SDK** (2026-11-04): `weft-client`, async on `reqwest`, covering the same routes. Stored values are written from any `StoredValue` (decimal text, `BigDecimal`, integers) and read back as `BigDecimal`; `f64` does not implement `StoredValue`, so the float boundary is a compile error rather than a runtime refusal. Cursor-following reads are `Stream`s, refusals are one `Error` carrying the whole envelope, and an overloaded read is retried after `Retry-After`. Features: `rustls` (ring provider, platform or supplied roots) and `arrow` (Arrow IPC and Parquet to `RecordBatch`). `tests/fake.rs` pins the client's own behaviour against a fake server; `tests/live.rs` is the contract test against a real `weft-server`, which CI runs on Linux after the build.
  - [x] **Arrow Flight** (2026-11-04): an opt-in, read-only gRPC listener (`WEFT_FLIGHT_ADDR`, experimental, loopback only since it is plaintext). `DoGet` takes a JSON ticket (`aspect` with `start`/`end`, or `lo`/`hi`) and streams the same typed record batch the Arrow range routes encode; `GetFlightInfo`, `GetSchema` and `ListFlights` describe the declared aspects. Keys and the `read` scope apply as on HTTP, denials are audited, a query gate sheds load as `UNAVAILABLE`, and refusals keep their envelope code in `x-weft-error-code`. `weft-server/tests/flight.rs` drives it with the Arrow project's `FlightClient`. Writes stay on HTTP.
  - [ ] **Flight SQL**: waits for the SQL surface (DataFusion, later): Flight SQL is SQL statements over Flight, and there is no SQL engine to answer them yet.
- [ ] Grafana → Prometheus remote write/read (if monitoring) → R/Arrow workflows
  - [x] **Grafana** (2026-11-04): `GET /api/v1/grafana`, `POST …/metrics` and `POST …/query` answer the community JSON datasource (`simpod-json-datasource`), experimental and in the `read` scope. A panel's targets are aspects; each is the stored downsample over the panel's range at its interval (capped at `maxDataPoints`), the reduction picked per target (`agg`, default `avg`). `packaging/grafana/weftdb-datasource.yaml` provisions it; the unit test in `weft-server/src/grafana.rs` charts a stored aspect through the router.
  - [ ] **Prometheus remote write/read**: a remote-write series is a metric plus a label set (DECISIONS 2026-11-04).
    *Unblocked 2026-11-05: B-tags landed (TAG-5..TAG-11, #552-#570), so a label set can map to tags on one aspect. Not started: there is no remote-write/read route yet.*
  - [x] **R/Arrow workflows** (2026-11-04): `clients/r` (package `weftdb`) reads stored windows, value bands and server-side downsamples as Arrow tables, pages the exact JSON rows, and writes data frames or Arrow tables as one Parquet batch with `Idempotency-Key`. Refusals are `weft_<code>` conditions. `tests/testthat/test-live.R` is the contract against a running server. It found that Parquet ingest refused Snappy, the default codec of R's arrow, pyarrow, pandas and Spark; the workspace's `parquet` now reads it.

### Phase 3 — Instrument everything · *Very high*

The three remaining instrumentation items closed between 2026-10-27 and 2026-11-03.

- **Stored-read spans:** every stored read now closes a `.serialize` span tagged with its `format`, and Arrow/Parquet encoding is timed as its own stage. `every_stored_read_format_closes_its_serialize_span` in `weft-server/src/storage.rs` pins this.
- **Seal-path accounting:** `publish_seal` records its index insert in a `control_plane.index.insert` span carrying `rows_changed`, asserted by `weftdb/tests/seal_index_span.rs`.
- **Bench-run routes:** the `/bench/runs` routes were first deferred because nothing named a run, and serving provenance before SEC-6/SEC-8 would have exposed it. They shipped in `weft-server/src/bench_runs.rs` with a content-addressed run id, the SHA-256 of the manifest file. With `WEFT_BENCH_RUNS_DIR` set, `GET /bench/runs/{id}` serves the manifest byte for byte with the id as its `ETag`. The routes sit in the `metrics` scope at the experimental tier, outside the 1.0 freeze.

### Phase 4 — Hot path: physical types & Storage v2 · *High*

**Time policy (4.1).** The tz and leap-second policy is stated on `TimeUnit` (`weft-physical-type/src/timestamp.rs`). Timestamps are POSIX UTC ticks, no zone is stored and no leap second is modelled. The policy also brought `TimeUnit::{nanos_per_unit, to_nanos, from_nanos}`.

**Quality pruning (4.3).** Point reads and downsample now skip all-null segments and pages:

- `SegmentIndexStore::prune_present_descriptors` drops an all-null segment before its file opens.
- `read_segment_point(s)` drops null frames and pages using persisted stats.
- Range reads and the last-writer-wins `read_segment_instant` are deliberately not pruned. A null row is part of a range answer, and skipping a segment can change which segment wins.

**Tag pruning (4.3, #158).** A single-series read now prunes in SQL through migration `0007_series_time_index`. That migration adds `idx_segment_index_series_time`, and each series keeps its own span bound. Series are resolved through a per-aspect catalog cache.

**Reconciliation (4.6).** Skip-identical reconciliation turns a replay whose fold matches its kept member into a delete-only `SwapPlan::keep`, so no frame is written. It is covered in the swap crash matrix as `Op::SkipIdentical`. Upsert keys ship in narrowed form, `timestamp + K`, via migration `0008_upsert_keys`. The keys are immutable and part of declare equality. `value` as a key and dedup-off are not offered, and DECISIONS.md gives the reason.

### Phase 5 — GPU interpolation flagship · *High (parallel w/ Phase 4)*

Every Phase 5 item merged by 2026-11-05. On 2026-11-04 the owner lifted the gates on multi-GPU, memory-mapped GPU I/O and the post-1.0 HTTP route, and splimes 1.3.0 was released, which WeftDB depends on.

Changes in WeftDB:

- **Streamed stored interpolation:** `GET /api/v1/storage/{aspect}/interpolate` (#572) amends decision 18. It streams JSON/CSV through splimes `stream_epoch` in bounded chunks, overlapping CPU and GPU work (#585). Arrow/Parquet stay buffered.
- **Hardware calibration:** `weft-server/src/gpu.rs` calibrates the hardware at startup for `Backend::Auto`.
- **Float handling:** the compute endpoints widen JSON `f64` through the shortest round-trip decimal (#553) and use `run_f64` for range output (#575).
- **New benches:** `--gpu-economics` (#569), the conformance matrix with tolerances in `docs/gpu-conformance.md` (#573) and the end-to-end p50/p95/p99 benchmark `weft-e2e-interp` (#586).
- **Opt-in features:** multi-GPU through `WEFT_GPU_ADAPTER`/`WEFT_GPU_DEVICES` (#584) and mmap segment reads through `WEFT_SEGMENT_MMAP_READS` (#587).
- **Memory soak:** `weft-bench --gpu-memory` records device memory and fails the run on monotonic growth (#583).

Measured in splimes on an RTX 4070 Ti SUPER, with results that led to dropping or demoting features:

- **5.1 command batching:** batched GPU beat per-call GPU 7–24×, but rayon across series beat it 3.5–19×, so the batching API was removed again (#12).
- **5.2 async handles:** they did no better than the existing `spawn_f64` (0.72–1.28× under load), so they were dropped too (#16).
- **Multi-GPU:** slower than one device in every case, `f64` by up to 3.6×, which is why it ships opt-in (#14).

None of the WeftDB-side code had been built when it merged, because of the owner's no-builds-before-merge rule. The build, tests, CI and every WeftDB measurement still run after the merge: 5.3, 5.5, 5.6, the E2E benchmark, mmap, the device-memory soak and the widening/`run_f64` benches. Until then no number is claimed for any of them. Details are in splimes #11–#16 and WeftDB #553–#587.

### Phase 6 — Compression v2 · *High*

The value and timestamp codecs have all been through the adopt-or-drop gate and the winners are realised on disk. Gorilla with RLE, dynamic per-block adaptive bit-packing, a scaled-integer value bit-pack and frame-of-reference (FOR, adopted for the value column with owner sign-off) are shipped, and per-block adaptive packing won on the BTC corpus. The f64 codecs (Gorilla, Chimp, Chimp128 and an Elf-style one) sit behind a best-of selector, delta-then-pack cascades are composed, and the FastLanes transposed layout is written to disk. The Sprintz-style FIRE predictor was measured and dropped, `pco` was benchmarked on both columns, and the headline bytes/point now reports the realised figure rather than the estimate.

The read-path work found where decode time actually goes: the scalar FOR/blocked decode at wide widths, the per-bit loops in the f64 codecs, and a ~2.4 ms floor under every read that turned out to be the CRC. Elf, FIRE and the other advisory f64 codecs are gated behind features, the bit-sliced value codec is gated pending counsel, and adapted code carries its attribution.

- [ ] **6.1 Lossless typed codecs**
  - [ ] **Codec patent/licence gate (owner decisions 2026-10-08).**
    - [ ] **Default FOR codec and the Tiger Data family: with counsel.**
    - [ ] **The 2026-10-08 routine's advisory codecs: not yet reviewed, and ungated.**
    - [ ] **SAP family: review before the Phase 6.2 design.**
    - [ ] **Possible future: a patent-clear codec in a new crate.**
    - [ ] **Package licence metadata: mitigated by the relicense; the expression is with
  - [ ] **Evaluate Pcodec as an integer/f64 codec + bench baseline — now with the upstream numbers
    - [ ] **SCOPE CORRECTION (research 2026-09-23): evaluate pco for the COLD TIER and as a
    - [ ] **The real prize in pco may be its MODE DETECTION, not its format.**
    - [x] **DONE (2026-10-08, advisory) — factor a timestamp column's common multiple (pco's `IntMult`) before DoD
      - [ ] **NEXT — realize the common-multiple factor as a timestamp codec flag (owner sign-off:
  - [ ] **f64-codec adopt: target ALP, not Chimp128/Elf — the state of the art.**
  - [ ] **ALP now has a FROZEN wire layout to mirror — do not invent a container (research
  - [x] **DONE (2026-10-08) — Use the `alp` crate to get the ALP adopt benchmarked, rather than hand-rolling it first.**
    - [ ] **ALP adopt as `VAL_CODEC_ALP` — owner sign-off (headline change).**
    - [ ] **NEXT — realize the decimal-exponent FOR as `VAL_CODEC_DFOR` (owner sign-off: headline
    - [ ] **Re-decide the transposed layout now that the linear baseline is honest — RECOMMEND
    - [ ] **Next read-path floor: every point read still CRCs the whole frame** (real-data evidence
  - [ ] **Set the ALP acceptance bar from upstream's own ablation, and be willing to DECLINE.**
  - [ ] **GPU-decode ALP — retarget to G-ALP (DaMoN'25), and decide the exception layout BEFORE the
  - [ ] **Fuse decompression INTO the GPU interpolation kernel — with a number behind the wedge.**
  - [ ] **Evaluate Vortex as a columnar interchange + benchmark reference (Phase 1/4):**
- [ ] **6.2 Model-based compression** (leverages WeftDB's spline DNA, NeaTS-like)
- [ ] **6.3 Late/compressed-domain execution**
- [ ] **6.4 Quality benchmarks**

### Phase 7 — Online perf, durability, correctness · *High for beta*

Durability slices S1 to S9 and S18 have landed: legacy MVCC logs preserved and no-op sync pragmas dropped, a durable I/O layer with fault points and a power-cut simulator, hardened store open, store-wide sweep isolation, atomic publish of backup directories with a manifest and retention, schema v2 with `IndexTxn` and write poison, a persistent per-aspect id allocator with per-aspect commit and maintenance locks, both write-once maintenance slices, and legacy rows-mode hygiene.

Backups are online: control-plane snapshots over `VACUUM INTO` with per-database verification, a background daemon (`WEFT_BACKUP_INTERVAL_SECS`) that prunes generated snapshots (`WEFT_BACKUP_KEEP`), and a verify mode that is safe under concurrent writes. Restore and a backup/restore drill exist, and the drill is exposed at the API.

- [ ] **7.1 Online ingest**
- [ ] **7.2 WAL & crash consistency.**
  - [x] **S10** Write-once durable seal with id assigned at commit *(~8 h; after S9)*
  - [x] **S11** Rollup into segment_index.db, folded in the seal and swap transactions *(~7 h; after S10)*
  - [x] **S12** Startup recovery, quarantine with TTL, fsck endpoint, /ready report *(~8 h; after S11)*
  - [x] **S13** Idempotent atomic ingest entry point (library) *(~7 h; after S12)*
  - [x] **S14** HTTP ingest wiring
  - [x] **S15** Partial sidecar v4
  - [x] **S16** Whole-store backup with hard-linked frames
  - [ ] **S17** Relaxed durability mode with synced_epoch watermark (optional for 1.0) *(~6 h; after S12)*
  - [x] **S19** Seal-backed legacy aspects
  - [x] **S20** Change log replaces the per-timestamp unbatched queue for seal-backed aspects *(~7 h; after S19)*
  - [x] **S21** Crash-matrix CI, subprocess SIGKILL soak, durability benchmark and ROADMAP re-baseline *(~7 h; after S14, S16)*
- [ ] **7.3 Corruption detection**
  - [ ] Restoring *into a chosen new root* over HTTP
    *The offline path exists: `weft-server restore <BACKUP> --into <NEW_ROOT>` (PAC-13, `weft-server/src/restore.rs:65`); only the HTTP route is missing.*
  - [x] Whole-store backup manifest (fold the `.weftseg` frames in beside the control plane)
    *Closed 2026-11-05 (audit): done 2026-11-01 (D-S16, #483). The `weftdb-whole-store-backup/1` manifest (`weftdb/src/types/backup.rs:412`) lists the hard-linked `.weftseg` frames beside the four control-plane databases; `restore_store` and `weft-server restore` read it.*
  - [x] Document RPO/RTO against the measured snapshot cadence
    *Closed 2026-11-05 (audit): `docs/RPO-RTO.md` (EVI-19, #545); the soak backup lane records RPO/RTO by those formulas (SOA-11, #578).*
- [ ] **7.5 Compaction**
- [ ] **7.6 Quotas/limits**

### Phase 8 — Commercial hardening · *Required for paid beta*

The licensing decisions are made. Every WeftDB crate is `MIT OR Apache-2.0`, Justin Icenhour is the sole copyright holder, kdb+/KDB-X is excluded from published benchmarks unless KX consents in writing, and the flagged opt-in codecs are gated until counsel signs off.

- [ ] Security
- [ ] Compliance readiness
- [ ] Packaging
- [ ] Observability
- [ ] SDKs — Rust → Python → TypeScript → R/Arrow
- [ ] Licensing/legal
  - [x] **Contribution terms: DCO or CLA, the contributor's choice.**
    - [ ] **Enforce it (owner, once on main):**
  - [ ] **With counsel:**
  - [ ] Still open

### Phase 9 — Analytics premium · *Medium, after benchmark foundation*

- [ ] Sliding windows → overlapping windows → event-centered windows
- [ ] Resample-before-compare
- [ ] Pattern dedup → occurrence-distance constraints
- [ ] Correlation / signal benchmarking
- [ ] Per-stage audit trail
- [ ] *(Future)* ML/AI

## 1.0 release criteria

### Durability & correctness *(the beta bar — Phase 7)*

- [ ] **7.2 WAL & crash consistency**
- [ ] **7.3 Corruption detection**
- [x] **7.4 Whole-store backup manifest**
  *Closed 2026-11-05 (audit): whole-store manifest `weftdb-whole-store-backup/1` shipped in D-S16 (#483).*
- [ ] **7.4 Documented RPO/RTO** against the measured snapshot cadence
- [ ] **7.5 Compaction** with query consistency, cancellation, and resource limits
- [ ] **7.6 Quotas/limits**
- [ ] **Phase 7 acceptance run**

### Hot path & data model *(must land before the freeze)*

`splimes` 1.0 is released and WeftDB depends on it, and the control-plane engine decision is made.

- [ ] **Measurement bulk ingest routed through the `.weftseg` seal**
- [x] **B-tags**

### Security & packaging *(Phase 8 minimum)*

- [x] TLS on `weft-server`
  - *Audit 2026-11-04 (SEC-13): checked — the acceptance suite (`weft-server/tests/security_acceptance.rs`) boots the real binary with a TLS certificate and a mounted keys file and serves the whole secured surface over HTTPS, with plaintext to the same port unserved.*
- [x] API-key/token auth + basic RBAC + audit log
  - *Audit 2026-11-04 (SEC-13): checked — the same suite pins the full role × scope matrix (SEC-3's six roles × eight scopes, enforced by SEC-6's keys), live key reload, and SEC-9's audit trail for a mixed run of denials and mutations, in-process and over the wire.*
- [ ] Packaging
- [ ] Upgrade/rollback procedure, exercised against the previous release

### Release engineering

Dependencies were brought current and audited on 2026-10-05, `SECURITY.md` is on main, and `cargo deny check advisories bans licenses sources` runs in CI.

- [ ] **CI**
- [ ] **Public surface declared and frozen**
- [ ] **Compatibility tests**
- [ ] **Semver policy + `CHANGELOG.md`**, starting from the first tagged pre-release
- [x] **Crate publishing split**
- [ ] **Panic audit on server paths**
- [x] **Security process**
  - [x] **Private vulnerability reporting is off**
    - *Audit 2026-10-27: checked — `gh api repos/basic-automation/weftdb/private-vulnerability-reporting` now returns `{"enabled":true}` (the heading text is historical). The other children hold: SECURITY.md routes reports to GitHub private reporting, and the `deny` job in ci.yml runs advisories, bans, licenses and sources.*

### Claims & documentation

- [ ] Every performance claim in the README links to a benchmark artifact (the [governing rule](#weftdb-roadmap))
- [x] First cross-engine report
- [ ] Honesty pages shipped (see [Commercial thesis](https://basicautomation.io/projects/weftdb/docs/commercial-thesis))
- [ ] Legal review of competitor benchmark-publication terms + codec licenses (see [Legal review](#legal-review))
- [ ] README *Project status* table updated to reflect 1.0 and the status badge changed from `pre-beta`

### Execution waves (W0–W15)

Later waves landed the v0.1.0 baseline release (PAC-4, a GitHub release only) with one fixture set written by that binary (X-FX), and the stale draft prerelease was deleted. The server and store lanes followed: the `manage.rs` and `main.rs` split (X-SPLIT-SRV), write-once maintenance II (D-S9), the configuration registry skeleton (PAC-5a), the single error envelope and code table (X-ERR), `aspect_name::encode` and `validate_new` at declare (X-NAME), storage-engine panic fail-stop (ROB-2), and the frame registry with its v7 envelope (X-FRAME).

On packaging, PAC-1b added the stripped dist profile with a third-party licence bundle, and PAC-3 stopped the library crates imposing a global allocator or wgpu features on their users. The remaining waves and open lanes are listed below.

- [x] **W0 · Land in-flight work and lock the shapes S6/S7 depend on**

  In-flight work is merged: INT-0 landed the splimes-1, legal and durability branches into main. The store lane's shapes are locked and implemented: Storage-v2 open hardening (D-S3), atomic backup publish with manifest, retention and drill hygiene plus the backup-label grammar (D-S5, SEC-1), legacy rows-mode hygiene (D-S18), schema v2 with `IndexTxn`, write poison and root-relative frame paths (D-S6), and the persistent per-aspect id allocator with commit and maintenance locks and plain INSERT for seals (D-S7). Decision 1 records the store shapes S6 and S7 rely on.

  On the release and identity side, the release pipeline (PAC-1a) and the canonical `TagSet`, `SeriesKey` and `SeriesSelector` types with their frozen caps (TAG-1) are in, and GitHub private vulnerability reporting is turned on (owner step).

- [x] **W1 · Baseline release, server split, goldens**

  Release truth is settled: per-crate versions, a single edition, the publish split and package hygiene (FRE-1). Golden frames, sidecars and persisted-encoding goldens, gated codecs included, are committed (FRE-3), and weftdb's `ValueKind` is open with a provenance scope (TAG-3). The README's commands, flags, endpoints and links were checked against the code on 2026-10-25 and its one over-claim reworded (EVI-1). Bench runs carry a manifest at schema v23 (EVI-3). `weft-bench --sparse` measures a seeded nullable grid and records the observed missingness, and `--scale tiny|small|medium|large` sizes the sparse, gap-fill and downsample workloads and names the preset in the profile (EVI-9). The soak harness skeleton is in (SOA-1).

- [x] **W2 · Error contract, frame format, portable builds**

  The legacy lane closed FRE-14c: `weft-server import-rows-mode` streams one 0.1.x subject into a Storage v2 store, migrates `metadata.db`'s non-row state, reports what stays behind and refuses a second run. Linux binaries are portable on a glibc 2.28 baseline (PAC-2). A nightly regression workflow and `weft-bench compare` watch performance (EVI-5). `bench/claims.json` vouches for every README figure and `tools/check_claims.py` enforces the markers, sources and links in CI (EVI-2), with the artifact pipeline behind it (EVI-4). The ingest workload over SegmentStore with the UCI loader (EVI-6) and benchmark adapter v2 I (EVI-8A) landed. On the soak side: preflight, device resolution, run metadata and secret redaction (SOA-2); the open-loop scheduler, HDR interval logs, self-monitoring and null server (SOA-3); and the deterministic workload model with format-exact value grids and body encoders (SOA-4).

- [x] **W3 · Gate, serve path, input domain**

  The store lane added library stored-range interpolation with input caps (EVI-10A) and the rollup in `segment_index.db`, folded into the seal and swap transactions (D-S11). Routes live in a sealed registry with a frozen scope table and a fail-closed gate (SEC-3), served by one HTTP/1.1 path with pre-auth listener limits that absorb ROB-23's caps (SEC-4). Decimals are bounded (ROB-6). Benchmark adapter v2 II (EVI-8B), the soak ingest client (SOA-5) and the Linux system samplers with contamination flags and the strict-mode flush check (SOA-12) round it out.
- [x] **W4 · Auth, config, ILP fix**

  The store gained an idempotent atomic ingest entry point with `groups`, ledger `series_ids` and typed errors (D-S13). The server got a rustls/ring TLS listener on the shared serve path (SEC-5), API keys with Bearer auth, a keys file and keygen (SEC-6), panic containment with daemon supervision, the Daemons registry and `weftdb::exec` (ROB-3), and strict configuration with a store filesystem check (PAC-5b; the bind guard moved to SEC-8). ILP series merging is fixed (TAG-0) and frame decoding hardened (ROB-8). The stored WeftDB benchmark adapter (EVI-11) and the soak oracle core (SOA-6) landed.
- [x] **W5 · Ingest wiring, whole-store backup, legacy seal**

  The store lane added the partial sidecar v4 (D-S15) and seal-backed legacy aspects (D-S19), and L-INGEST wired ingest through. The no-panic lint, asserts included, is enforced and weft-arrow exposes a `Result` API (ROB-4); admission I is in (ROB-10). Startup ordering, the exposure matrix, the auth-off guard and metrics auth (SEC-8) shipped with exposure and secrets hygiene (SEC-10). The compute vocabulary uses exact-case tokens with no silent narrowing (FRE-5), and declarations are immutable with scale rules and schema read-back (FRE-7b). DuckDB adapter I (EVI-13), the report renderer with generated honesty sections (EVI-15R) and honesty pages I (EVI-16) landed, as did soak window digests with an oracle self-test (SOA-7) and the failure deck with phases, recovery windows, the SIGTERM verdict and the maintenance driver (SOA-8).

- [ ] **W6 · Durability CI, store split, scheduler**
  - [x] **D-S21** Crash-matrix CI, subprocess SIGKILL soak, durability benchmark and re-baseline, with **SOA-9** (`crash_kill9`)
  - [ ] Run D-S21's privileged power-loss validation (sudo: dm-log-writes or a QEMU hard reset) *(owner)*
  - [x] Store: **X-SPLIT-STORE** Split segment_store.rs into `open`/`read`/`seal`/`maint`/`stats` submodules
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
  - [x] **EVI-25** Backup, restore and recovery cost measurement (RTO inputs and backup duration)
- [x] **W7 · Post-split lanes open; security and packaging close**

  The open lane pinned the store scope: a start whose `WEFT_STORE_DATABASE`/`WEFT_STORE_SUBJECT` differ from the root's `STORE_FORMAT` exits 65 unless `WEFT_STORE_SCOPE_PIN=off`, and `GET /api/v1/version` answers build, layout, scope-pin and exit-code state from process state alone (FRE-12b; DECISIONS 2026-11-03). The read lane bounded stored downsample and point lookups (ROB-13). Ingest rejects non-finite input as `400 not_finite` in every format and enforces `WEFT_MAX_INGEST_ROWS` and `WEFT_MAX_INGEST_DECODED_BYTES` before the seal (ROB-7, #551). GPU configuration, compute limits, interpolation provenance and metrics (ROB-16), the probe-only admin listener (ROB-23r) and the legacy queue-consumer panic fix (ROB-5) landed. Security acceptance closed (SEC-13). Packaging shipped the offline `weft-server restore` CLI (PAC-13), the Docker image with a container smoke test (PAC-9), and the systemd unit with host-install CI (PAC-11). The first cross-engine report and its owner-approved narrative (EVI-15), documented RPO/RTO (EVI-19) and the soak backup lane with final restore verification (SOA-11) close the wave.
- [ ] **W8 · Tags core, memory budget**
  - [x] open lane
  - [x] → **TAG-9** Frame error classes in recovery
    *Closed 2026-11-05 (audit): #556 (2026-11-04). Recovery and fsck adopt series-key frames into their series, class a malformed/empty/duplicate SERIES_KEY after a passing CRC as suspect (C5), rebind mismatched keys (`series_rebound`), and settle every held row (`held_cleared`).*
  - [x] → **TAG-5** Tags core: series table, catalog, and the layout marker
    *Closed 2026-11-07 by audit (#619): the series catalog and its time index are registered
    migrations `0006_tag5_series` and `0007_series_time_index`, and the rest of the C1 columns
    landed earlier in the same registry (`0002_s6_s7`, `0004_d_s12`, `0005_d_s13`). No
    `0009_*` and no bump: `SUPPORTED_LAYOUT` stays 2 and layout 3 stays unassigned, because every
    shape of a marker-only migration either makes 0.3.x binaries refuse a store that differs only
    by a re-run of `CREATE TABLE IF NOT EXISTS series` or teaches that a layout number can move
    without meaning anything. DECISIONS 2026-11-07; the owner still owes the ruling on whether the
    literal `layout_version = 3` is a hard 1.0 gate.*
  - [ ] seal lane
  - [x] → **ROB-17** Disk guard, headroom API and space gauges
    - [x] Log the `507 insufficient_storage` refusal once per transition into and out of refusing, not once per request: today every refused ingest logs at ERROR, so a client retrying against a full disk floods the log (the `weft_disk_guard_refusing` gauge already carries the state).
    - [x] Report the guard's state on `/ready` (a `disk_refusing` field, still `200`) so a balancer or ingest router can steer writes elsewhere while reads keep being served — VictoriaMetrics' `vmstorage` does exactly this: it switches to read-only below `-storage.minFreeDiskSpaceBytes` and `vminsert` re-routes data to the remaining nodes (https://github.com/VictoriaMetrics/VictoriaMetrics/blob/master/docs/victoriametrics/Cluster-VictoriaMetrics.md#readonly-mode).
    - [ ] ROB-18 phase 1: call `weftdb::DiskGuard::admit` with the reserved body size, so a request is refused `507` before its body is read (plan C11; the guard is public for it).
  - [x] maint lane
  - [x] read lane
  - [ ] **ROB-14** Cgroup-aware memory budget with per-request reservations
  - [x] **FRE-8b** Stored-value text and one float-input rule across every ingest format
  - [x] **FRE-9a** OpenAPI generated from the route table, committed, drift-checked in CI
    *Closed 2026-11-07 (#615): generated from the real `RouteTable` via
    `openapi::from_table`, wired as the `weft-server openapi` subcommand
    (PAC-7 exit-code contract), committed at `compat/openapi/weftdb.json`,
    drift-gated by `generated_spec_matches_committed_file`, and structurally
    validated by `generated_spec_passes_structural_validation`.*
  - [x] **ROB-9** Mutation harness for frames, sidecars and Parquet
    - [ ] Report the three `parquet` 60.0.0 decoder panics upstream to apache/arrow-rs (`offset + len out of bounds`, `column start and length should not be negative`, `Decoder for dict should have been set`; reproducers are one-byte edits found by `weft-arrow-store/tests/parquet_mutation.rs`), plus an `arrow-buffer` 60.0.0 panic at `util/bit_chunk_iterator.rs:235` seen when 4,070 one-bit mutants were posted to a running server *(owner: outward-facing)*. WeftDB contains them in `weft_arrow::read_parquet` meanwhile (DECISIONS 2026-11-05). Before filing, check them against the two malformed-file panics upstream already closed in 2026 — a compact-Thrift bool `unwrap` (https://github.com/apache/arrow-rs/issues/9839, closed 2026-04-28) and a row-count/column-data mismatch index panic (https://github.com/apache/arrow-rs/issues/9992, closed 2026-05-22) — since ours were seen on 60.0.0 and are different messages.
  - [x] **SEC-14** Security documentation, config schema and upgrade notes
    *Closed 2026-11-09: `docs/security.md` rewritten with source citations, full role table,
    credential-in-query guard, proxy guard, exit status table, startup enforcement, and
    known limitations. `docs/upgrade-notes.md` already existed; `docs/config-schema.md`
    updated by a concurrent PR.
  - [x] **PAC-10** Publish the multi-arch image (only after API auth)
  - [ ] Make the ghcr.io weftdb package public after PAC-10's first push *(owner)*
  - [x] **EVI-17** Honesty pages II
  - [ ] Write or approve the honesty-page narrative *(owner)*
  - [x] **FRE-13** Layout-1 to layout-2 upgrade test
    *Closed 2026-11-09 (#666): `weftdb/tests/layout_upgrade.rs` covers the [layout 1 → 2 upgrade contract](https://github.com/basic-automation/weftdb/pull/666) through the public API.*
  - [x] **SOA-13** SoakReport, SLO evaluator R1–R8, coverage floors, honesty section
    *Closed 2026-11-07 (#603): `SoakReport`, the R1–R8 evaluator, the coverage floors and the
    honesty section are on main; the report is what #616's nightly comparison reads.*
  - [ ] → **SOA-15** soak.yml nightly workflow, TTL lane and nightly-to-nightly comparison
    *Updated 2026-11-09: parts 1 (nightly workflow with active cron `0 7 * * *`) and 3
    (nightly-to-nightly comparison with first-night-no-baseline path) shipped in #616 and
    verified present on `origin/main`. Part 2 (TTL lane) remains blocked: the store has no
    row-level TTL or expiry mechanism (`weftdb/src/types/segment_store.rs:1982-1992` only
    purges expired *quarantine* records; `weft-bench/src/soak.rs` uses `NullServer` so no
    server endpoint can be exercised). Adding a TTL lane requires a store-side TTL feature
    first. The workflow already reports the TTL lane as `"not tested"` in its honesty section
    (`weft-bench/src/soak_report.rs` lines 610-611).*
- [ ] **W9 · Read semantics, limits, upgrade legs**
  - [ ] read lane
  - [ ] maint lane
  - [ ] open lane
  - [ ] L-INGEST
  - [x] storage read handlers lane (TAG-10)
  - [ ] **FRE-10a** Contract tests
  - [x] **PAC-14a** UPGRADING.md and compat legs A (upgrade) and C (rollback after writes) (absorbs FRE-19)
  - [x] **SOA-16** Soak upgrade lane
- [ ] **W10 · Tag gate flip, compaction closes**
  - [x] **TAG-11** HTTP ingest with tags and the tagged-write gate flip
  - [ ] **ROB-21** Maintenance cancellation, RAII cleanup guards, minimal maintenance metrics
  - [ ] read lane
  - [x] **FRE-15** STABILITY.md, semver policy and the release-criteria wording
    *Closed 2026-11-08: [`docs/stability.md`](docs/stability.md) documents the semver
    policy, surface stability table, compat-suite coverage and named gaps; linked from
    README.md and docs/reference.md.*
  - [x] **PAC-15** Operator documentation
- [ ] **W11 · Contract candidate, beta tag, calibration**
  - [ ] Move the T7 reference drive (`/mnt/weftbench`) to a 10 Gbps port before calibration *(owner)*
  - [ ] **FRE-9b** OpenAPI
    - [x] Extend the release tag grammar to `vX.Y.Z-beta.N` (`TAG_RE` in `scripts/release/verify.sh` accepts only `vX.Y.Z` and `vX.Y.Z-rc.N` today, so a beta …
    - [ ] Approve and push the **v1.0.0-beta.1** tag on FRE-9b's merge *(owner)*
  - [ ] → **SOA-17** Calibration on a pre-grown store, frozen acceptance profiles, `scripts/crash-soak.sh`
  - [ ] **FRE-10b** Contract tests
  - [ ] → **FRE-16** CI compatibility gates
  - [ ] **TAG-12** Multi-series crash matrix, fan-out and TSBS benchmarks, fixtures, freeze docs
  - [ ] **ROB-24** Limits and overload evidence
- [ ] **W12 · Phase 7 acceptance**
  - [ ] **SOA-18** 24 h acceptance run on the reference SSD, published as an artifact
  - [ ] Run the privileged power-loss validation on the acceptance commit *(owner)*
- [ ] **W13 · Freeze**
  - [ ] Settle the [Phase 8](#phase-8--commercial-hardening--required-for-paid-beta) counsel items (codec patents, **WEFTDB** trademark, benchmark policy) before rc.1 *(owner)*
  - [ ] **FRE-18** rc.1 freeze cut and layout-2 baseline fixtures
    - [ ] Approve and push the **v1.0.0-rc.1** tag on FRE-18's merge *(owner)*
- [ ] **W14 · RC verification on release artifacts**
  - [ ] **SOA-19** RC soak on the rc release artifact in the shipped default config (8 h, or 24 h)
  - [ ] **EVI-21** 1.0 evidence run at the rc
  - [ ] **PAC-18** Dress rehearsal
- [ ] **W15 · 1.0.0**
  - [ ] Write or approve the 1.0 release notes and upgrade notes before the cut *(owner)*
  - [ ] **EVI-22** README 1.0 project status, badge and versions
  - [ ] → **PAC-18** Cut 1.0.0
    - [ ] Approve and push the **v1.0.0** tag *(owner)*

## Predecessor-derived backlog

The interpolation item (B-interp, Phase 5) is done.

- [x] **B-tags** *(Phase 2/4 · done: TAG-5..TAG-11)*
- [ ] **B-conn** *(Phase 2/7 · absent)*
- [ ] **B-ilp** *(Phase 2 · partial)*
- [ ] **B-rest** *(Phase 2 · partial)*
- [ ] **B-poll / B-retry / B-register** *(Phase 7 · absent)*
- [ ] **B-analysis** *(Phase 4/9 · verify)*
- [ ] **B-object** *(Phase 4 · absent)*
- [ ] **B-windows** *(Phase 9 · absent/partial)*
- [ ] **B-dedup** *(Phase 7 · partial)*
- [ ] **B-sim** *(Phase 9 · partial)*
- [ ] **B-precision** *(Phase 4/6 · partial)*
- [ ] **B-resilience** *(Phase 4/7 · partial)*
- [ ] **B-conncache** *(Phase 4/7 · partial)*
- [ ] **B-msgpack** *(Phase 6 · absent)*
- [ ] **B-snapshot** *(Phase 3/9 · absent)*

## Control-plane engine: Turso/libSQL 0.6 adoption

Turso is migrated from 0.4 to 0.6 and then to 0.8.1 (2026-10-05), with the control-plane schemas moved to plain `INTEGER PRIMARY KEY` for MVCC. The 0.7 upgrade evaluation is settled. Online backups use `VACUUM INTO` with a concurrent-write-safe verify and a background daemon, and snapshot directories no longer accumulate empty `.db-log` and `.db-wal` sidecars. `Statement::n_change()` affected-row accounting is shipped for the aspect-metadata write and the seal's index write (Phase 3).

The backup cost was measured on 2026-09-23, and the diagnosis on record at the time turned out to be wrong; the corrected one is in the docs. Everything still listed below is control-plane-only by design: none of it turns Turso into the measurement backend.

- [ ] Lean on production MVCC concurrent writes for concurrent ingest + catalog updates under load *(Phase 7)*
- [ ] Encryption at rest (AEAD pager) for catalog/metadata/pipeline-state DBs *(Phase 8)*
- [ ] **Backup 7.4 residue (b) — incremental / WAL-streaming backup** beyond the full `VACUUM INTO`
- [x] **Backup 7.4 residue (d) — whole-store backup manifest**
  *Closed 2026-11-05 (audit): shipped in D-S16 (#483) with hard-linked frames (`weftdb/src/types/backup.rs:412`).*
- [ ] **Backup cost residue (a) — the root cause is documented upstream, and it is per-call
- [ ] **Backup cost residue (b) — (i) MEASURED 2026-10-08: `synchronous=OFF` is NOT a material win.**
- [ ] **Backup cost residue (c) — DO NOT reach for Turso's experimental passive checkpoint.**
- [ ] **Backup 7.4 residue (b) is UPSTREAM-GATED, not an open design question (research 2026-09-23):**
- [ ] **Backup cost residue (d) — evaluate an `sqlite3_rsync`-style page-delta replica.**
- [x] **Confirm the snapshot verifier does not depend on MVCC-broken pragmas.**
  - *Audit 2026-10-27: checked — `the_verifier_never_relies_on_pragmas_mvcc_reports_as_zero` (weftdb/src/types/backup.rs:1250) statically asserts no verifier code names `page_count`/`user_version`/`application_id`/`freelist_count` and behaviourally checks both `VerifyMode`s count real tables/rows on an MVCC source.*
- [ ] Triggers (`BEFORE/AFTER/INSTEAD OF` + `WHEN`)
- [ ] Dynamic auth tokens as closures
- [ ] `UPDATE … FROM`, aggregate `FILTER`, `INDEXED BY`, `NULLS FIRST/LAST`
- [ ] Evaluate (don't rush)

## Six-month execution plan

- [ ] **Month 1 — Benchmark truth:**
- [ ] **Month 2 — Public API & observability:**
- [ ] **Months 3–4 — Hot-path optimization:**
- [ ] **Months 4–5 — GPU flagship:**
- [ ] **Month 5 — Compression v2 prototype:**
- [ ] **Month 6 — Commercial beta:**

## Legal review

Three decisions are made and recorded: kdb+/KDB-X is excluded from every published benchmark unless KX consents in writing, WeftDB's own terms are `MIT OR Apache-2.0` with contributions by DCO sign-off or the CLA, and the flagged opt-in codecs are gated. The counsel items are still open below.

- [ ] Review each competitor's benchmark-publication terms (kdb+ especially)
  - [ ] **Open with counsel:**

## Immediate next actions

The decimal-versus-f64 gap was measured at about 43x rather than 2x, which set the course for the integer-native path: `weft_reduce::reduce_scaled` and the scaled partials, the `avg` division floor, per-bucket bookkeeping off the integer path, time-weighted averages on integers, exact percentiles by selection, and sketch improvements. All of it is wired into `SegmentStore::downsample_range`. The bench harness times the integer path too and runs the compression, point-lookup, range-fetch and downsample workloads on real corpora, and the storage generators were found to flatter, so they now carry realism knobs.

Around that, the read-path arc is done: streaming and batch point lookup (also at the API), closed-form and block-random-access timestamp search, and windowed range reads. The `weft-reduce` crate has mergeable DDSketch quantiles, cross-segment downsample at the store layer and the API (4.7x with concurrent segments), per-segment `.weftpart` partials with a hierarchy of materialised tiers, and size-targeted compaction with a fragmentation gate in the reconcile daemon. Test infrastructure is healthy: the flaky GPU tests, the rustc ICE and the link OOM were root-caused and fixed, `cargo test --workspace` completes (969 passed), and the BTC bulk-load test is runnable. The codec and reconciliation items from Phases 2 to 6 that were tracked here are summarised in those phases.

- [ ] **README `## Status` is stale (noticed 2026-11-05):** it says v0.2.1 (the release is 0.3.0) and lists tags and client SDKs as "Not started", but B-tags (TAG-5..TAG-11) and the Python, Rust and R clients have shipped. Owner-facing wording; EVI-22 rewrites the table at 1.0, but it is wrong today.
- [ ] **START HERE (filed 2026-11-05 for the next run).** W8 after ROB-17 and ROB-9 (#311, #315): the unclaimed non-owner slices left are ROB-14 (cgroup-aware memory budget), FRE-8b (stored-value text and one float-input rule), FRE-9a (OpenAPI from code), FRE-13 (layout-1 → layout-2 upgrade test), SEC-14 and SOA-13. Check #125 and open PRs for claims first. Builds: the shared `Cargo Target` lock was held for 30+ min by other projects' tests on 2026-11-05; the warm `/mnt/weftbench/target-ci` worked, and clippy needs `RUSTC_WRAPPER=` because the global nightly-threads wrapper breaks `clippy-driver`.
- [x] **Two `weft-server` bit-exactness tests failed under `cargo test -p weft-server` (found and fixed 2026-11-05):** `interpolate::tests::ilp_values_feed_the_f64_path_unchanged` and `stored_interpolate::tests::a_multi_chunk_stream_is_the_library_result` were 1 ULP off because serde_json's default float parser is not correctly rounded, and only a whole-workspace build (where bigdecimal's `serde-json` feature turns on serde_json's `arbitrary_precision`) parsed correctly. The release workflow and the Docker image build `-p weft-server`, so the shipped binary read some client JSON floats 1 ULP off. Fixed by serde_json's `float_roundtrip` workspace-wide, pinned by `num::json_float_parse_tests`.
  - [x] Measure what `float_roundtrip` costs the float parse itself: on 1,000,000 values of the compute routes' shape (magnitudes 1e-19..1e15, ryu-formatted, a 20.7 MB body), serde_json 1.0.151 release build pinned to one core of a Ryzen 9 7950X3D under load: 40.0–41.7 ns/value without the feature and 57.0–59.2 with it (≈ +17 ns, ≈ 1.42×, 8 runs each), against 175,103 → 0 values that failed to round-trip. The harness was a scratch crate; the correctness half is committed as `num::json_float_parse_tests::a_seeded_sample_of_json_floats_round_trips_exactly`.
  - [ ] What that costs a whole JSON compute or ingest request (a `weft-bench` run of the JSON interpolate route before/after): the parse is one stage of several, and no route-level figure has been taken.
  - [x] Run `cargo test -p weft-server` standalone in CI next to the workspace run (`features` job in `ci.yml`, `--lib`): the release binary is built per package, and feature unification across the workspace hid this bug from every workspace-wide test.
  - [x] Also test `weftdb`'s lib standalone there: 570 passed in 132 s locally with `SKIP_SLOW_TESTS=1`, including the per-package compile.
- [ ] **Flaky under GPU contention:** `weft-bench` `gpu_memory::tests::the_pool_stays_bounded_and_stable_on_a_real_gpu` failed in the 2026-11-05 full suite (`stable: false`, one buffer created after round one while 14 GB of the GPU was in use by other work) and passed alone.
- [ ] **START HERE (filed 2026-10-11 for the next run).**
- [ ] **Research (2026-10-11) — the exact TWA is now a measurable differentiator.**
- [ ] **START HERE (filed 2026-10-09 for the next run).**
- [ ] **(2026-10-08 list.)** The 2026-10-08 run (PR `routine/dsp-2026-10-08`)
- [ ] **(2026-09-23 list; item 3 shipped 2026-10-08.)** The 2026-09-23 run closed both cheap backup
- [ ] **POSITIONING ALERT — WeftDB's precision wedge now has a direct competitor (research 2026-09-23).**
  - [ ] **NEXT — the remaining 6.3×:**
    - [x] **DONE (2026-10-09, sub-items below) — the `avg` division was the biggest piece of `reduce_scaled`:**

      One idea, 19 digits per `u128` division, was tried and gave no win. What worked was a single `BigUint` multiply and one division per non-terminating average.

  - [ ] **NEXT — the sidecar build.**
  - [ ] *(original item)* **a `ScaledI64`-native reduction fast path in `weft-reduce`:**
- [x] **DONE (2026-11-02) — External-engine adapter fairness — three concrete rules from the 2026 landscape** (#393): (a) QuestDB over QWP both ways; (b) each ASOF strategy run and published (`clickhouse-double-merge`, `questdb-double-linear`); (c) a precedent, no work
- [ ] **Adopt the TSM-Bench QUERY SPEC (not the repo) as Weft-Bench's external comparison workload.**
- [ ] **BENCHMARK HYGIENE — the synthetic ingest corpus flatters WeftDB, by a lot (measured 2026-07-23).**
  - [ ] Audit the other `weft-bench` workloads' generators for the same class of flattery (the shape
    - [ ] **NEXT — a representative storage generator, or real-corpus defaults.**
    - [ ] **FOUND (2026-10-09) — `sketch_p99`'s published ~2.6× speed-up over exact `p99` was the
      - [ ] **Research (2026-10-09) — a cheaper sketch index, if the sketch format is ever revised.**
      - [ ] **Positioning (2026-10-09) — exact percentiles are a differentiator against QuestDB.**
      - [ ] **NEXT — select on the mantissas when a partial is not merged.**
- [ ] **The three unguarded `weft-orchestration` failures are latent bugs, not flakes — fix them
- [ ] **Re-measured 2026-10-09 — still unbounded.**
- [ ] **Bound the seven slow `weft-orchestration` tests the way `db_tests` was bounded.**
- [ ] **`weft-orchestration`: per-pipeline signal state, not the global `SIGNALS_QUEUE` (filed 2026-11-06, from the fan-controller build).** `SIGNALS_QUEUE` is a process-wide static (`lib.rs:662`); `Pipeline::generate_signals` and `filter_signals_at` clone the whole queue into each pipeline (`pipeline.rs:964`, `:982`). With several sensor pipelines in one process, one sensor's signals reach another's probability queries. Fix: key the queue by aspect (or move it onto `Pipeline`), and add a two-pipeline isolation test. Check whether `create_signals` clears the queue; if not, even running the pipelines one at a time leaks.
- [ ] **`weft-orchestration`: bounded-window threshold detectors (filed 2026-11-06, from the fan-controller build).** `detect_threshold_crossing_up` (`detectors.rs:504`), and probably its sibling detectors, analyse the aspect from its earliest to its latest measurement on every call, then collect and sort every point. For a 1 Hz daemon that runs for weeks, each tick costs O(history). Fix: an optional `since` start time (or an incremental cursor from the last evaluated point), so a live caller only scans new data. Add a test that the windowed and full-history runs find the same crossings in the window.
- [ ] **`weftdb` dictionaries: shrink the occurrence footprint without dropping any occurrence (filed 2026-11-06, after the Fan Control 112 GB incident).** Every occurrence must be kept, because the statistics need them all. No `max_patterns` or `max_occurrences` caps: if the dictionary needs a cap, it is under-constrained. The duplicate-on-every-run bug is **fixed 2026-11-07 (#612, 0.3.2)**: `Pipeline::import_windows` rewrites every id, so the old `if exists { continue }` test could not fire and each run re-inserted every occurrence. What remains:
  - **Lossless compact encoding.** Each `pattern_occurrences` row now repeats `pattern_id`, `aspect_id`, `resolution`, `size`, a JSON `database_info` and two timestamps. Store the per-dictionary constants once. Derive `end` from `start + size·resolution` and store it only when it differs. Store `start` as delta-of-delta varints, reusing the `weft-physical-type` timestamp codecs. Keep each pattern's occurrences in an append-only log on disk.
  - **Lean patterns in memory.** A pattern in RAM holds its shape, `count` and `first_seen`/`last_seen`. Occurrences are never loaded for matching; statistics and correlation read them as a stream. First check what `correlation.rs` needs from them.
  - **Restore on reload.** `load_dictionary` restores the stored patterns verbatim, with no re-matching through `import_pattern` and no occurrence load.
  - **Keep merging into every matching pattern**, as today. Any statistic should be computed per window, not by summing pattern counts.
  - **Learn the constraints when they are unset.** One shared radius λ, adjusted toward a target rate of new patterns. Each variability kind gets T_k = λ·(online median of that kind's nearest-neighbour distance). Steps and interpolation are learned together once, then frozen. `Steps.Enforced` defaults to true. Sum kinds are derived as N·avg. Percentile kinds are used only when |mean| ≫ noise. The packing number bounds the pattern count; warn if it keeps climbing.
  - **Tests:** replay the incident (1 window per minute) and check that `preamble.db` grows only linearly, at the compressed rate. Check that save then reload restores the dictionary exactly, and that there are zero duplicate occurrence rows.
- [ ] **`weftdb` memory per database file: RSS keeps climbing across many small stores (filed 2026-11-08, from the Fan Control daemon on 0.3.2).** The daemon's store holds 529 database files, and its RSS keeps rising after the dictionary-doubling fix. Measure before changing anything; two hypotheses:
  - **Page cache.** `configure_database_for_mvcc` (`database/connection.rs:119`) sets `PRAGMA cache_size=-256000` (~256 MB). If Turso applies it per *database*, 529 files allow about 135 GB, filled as pages are touched and never evicted below that. But it runs on a throwaway connection, and the comment above it says per-connection pragmas set there change nothing, so it may not apply at all. Confirm how `turso_core` scopes `cache_size`.
  - **MVCC version store.** Every file opens in `journal_mode=experimental_mvcc`, which keeps row versions in memory until a checkpoint. Many files that are written often and checkpointed rarely would show the same slow climb.
  - **Fix direction:** a process-wide memory budget split across open databases, not a fixed size per file. A small default `cache_size` (a few MB) on every working connection, raised only for bulk loads. Close or idle out databases that are not in use. Checkpoint the MVCC store on a schedule. Also question why one aspect-heavy app needs 529 files (one per aspect × dictionary): consolidating them is the bigger lever.
  - **Test:** open N=500 stores, write to each at 1/min for a simulated day, and assert that RSS stays under a configured budget and does not grow with N × cache_size.
- [x] **DONE (2026-11-10) — `weft-orchestration` is published, current at 0.3.2, verified through the crates.io sparse index (filed 2026-11-06).** `publish = false` would be a regression: it would withdraw a crate people already depend on. The remaining risk is the path/crates.io mixing hazard — two copies of `weftdb` in one binary, so the types do not unify — now documented in `weft-orchestration/README.md`: depend on **both** `weftdb` and `weft-orchestration` from crates.io, or vendor **both** by path, never one of each. `weft-physical-type`, `weft-reduce`, `weft-line-protocol` and `weft-arrow` sit at 0.3.0 while `weftdb`, `weft-arrow-store` and `weft-orchestration` are at 0.3.2; that lag is release hygiene, not a reason to unpublish.
- [x] **Historical note — the LNK1102 research that produced the `[profile.test] debug = 1` fix:**
  - *Audit 2026-10-27: checked — this is a historical note and its fix shipped: `[profile.test] debug = 1` is at Cargo.toml:128.*
- [ ] **Guard the `#[serial]`-above-`#[test]` ordering (research 2026-07-20):**
- [ ] **NEXT — UDDSketch (uniform guarantee under collapse):**
  - [x] The sketch itself, as a library type (2026-11-11): `weft_reduce::UddSketch` (`weft-reduce/src/uniform.rs`). Tests pin the uniform bound after collapse against an exact sort, the closed form of the bound, and exact merging under collapse.
  - [x] A bench for the cost against `DdSketch` (2026-11-11): `cargo bench -p weft-reduce --bench sketch_collapse`, two seeded synthetic inputs of 100,000 samples at 1%. First figures, criterion medians on a Ryzen 9 7950X3D **while the host ran at a load average near 100**, so they are indicative and not a claim. `add`, random walk, no collapse at 2048 buckets: `DdSketch` 0.65 ms, `UddSketch` 0.44 ms (three runs each, within 1%); `UddSketch` took 1.83 ms in the one run before it had `DdSketch`'s last-bucket cache. `add`, eighteen decades at a 256-bucket budget, both collapsing: 6.7 to 9.6 ms against 6.2 to 7.0 ms over five runs, no clear difference. `merge` of 64 collapsed sketches: 0.67 to 0.75 ms against 78 to 101 µs. What the speed buys on that input: `UddSketch` collapsed four times and reports a 15.9% bound on every quantile, where `DdSketch` keeps 1% on the quantiles above its folded buckets and no bound below.
  - [ ] Repeat those figures on a quiet host with pinned cores before citing any of them, and add the error each sketch gives at p50 and p99 on a real corpus (`database/datasets/btc_1min.csv`) at a budget small enough to force a collapse.
  - [ ] Decide whether the `sketch_p*` reductions take it. At `SKETCH_MAX_BINS` = 2048 a realistic column never collapses, so the two differ only on pathological ranges; switching changes the `.weftpart` sidecar bytes (postcard is not self-describing), so it needs a sidecar version step. Owner-gated, like the other default-codec changes.
- [x] **DONE (2026-07-18) — materialize per-segment partials at seal time (the highest-value

  Per-segment `.weftpart` sidecars are written at seal time at a declared base resolution. `downsample_range` merges the sidecars instead of decoding whenever every requested reduction allows it, a fine base can be re-bucketed to a coarser resolution, and the sidecars stay consistent across segment rewrites.

- [ ] **NEXT — (3c) materialized tiers, remaining:**
- [ ] **Positioning — WeftDB's `.weftpart` sidecar IS the "incremental materialized view", and mergeable
- [ ] **NEXT — materialize the sidecar at a coarser DEFAULT + adopt sketches:**
- [ ] **NEXT — default target for the compaction daemon:**
- [ ] Add the **DuckDB** adapter
  - [ ] **Research (2026-10-09) — DuckDB has a native linear-interpolation arm:**
- [ ] Add ClickHouse, InfluxDB 3, QuestDB, TimescaleDB adapters
- [ ] Add end-to-end timing spans *(harness-level spans shipped; per-pipeline-stage spans = Phase 3 tracing item)*
- [ ] **NEXT — upstream FastLanes does NOT make the transposed layout its default either, and
- [x] **DONE (2026-10-08) — Cross-check WeftDB's hand-rolled bit-plane decoder against the `fastlanes` crate (0.7.2,
  - [ ] **Residue — still 2.4–7× behind `fastlanes` (0.33 ms flat across widths).**
- [ ] **Track the FastLanes SPEC, not the CWI reference implementation.**
- [ ] **Next slice — evaluate Vortex as interchange + bench baseline (Phase 1/4):**
  - [ ] **GO on both counts, and the governance question is settled (research 2026-09-23):**
  - [ ] **Do NOT take a build dependency on `vortex-gpu`.**
- [ ] **Evaluate the FastLanes *File Format* (not just the layout) as a bench baseline + a
  - [ ] **It publishes a random-access target WeftDB can mirror exactly (research 2026-09-23):**
  - [ ] **Two honesty corrections from the same paper.**
- [ ] **Point-lookup positioning — WeftDB's block-random-access read is a genuine columnar mitigation
- [ ] **GPU-decode the `.weftseg` transposed layout rather than adopting a new format (Phase 5/6):**
- [ ] **Correlation-aware column compression as a post-B-tags codec direction (Phase 6):**
- [ ] **Cascade adopt-or-drop into the DEFAULT selector — owner-gated (headline change, as FOR/ALP):**
- [ ] **NEXT — checkpoint index adopt-or-drop into the DEFAULT policy — owner-gated (bytes/point
- [ ] **NEXT — a random-access-capable Gorilla/RLE would widen the checkpoint index's reach:**
- [x] Publish a methodology document **before** any performance claim
