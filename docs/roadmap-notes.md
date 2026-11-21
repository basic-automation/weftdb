# WeftDB roadmap notes

[`ROADMAP.md`](../ROADMAP.md) is the work queue: a phase-ordered checklist, one line per item, and the only place an item's status is kept. This file holds the prose that used to sit beside those checkboxes: phase records, measurements, wave narratives, audit notes, research notes and run handoffs. It was moved here verbatim on 2026-11-12, with the checkboxes turned into plain bullets, so nothing here is a status.

The text below uses the numbering the roadmap had before 2026-11-12: Phases 0–9 and execution waves W0–W15. Item IDs (`7.2`, `6.1`, `S17`, `FRE-9b`, `TAG-12`, …) did not change, so search for an ID to move between the two files.

| Old label | Phase in `ROADMAP.md` |
| --- | --- |
| Phase 0, Phase 2 core, 4.1, Turso adoption, compression codecs shipped, read-path and reduction arcs | Phase 1 |
| W0 | Phase 2 |
| W1 | Phase 3 |
| W2 | Phase 4 |
| W3 | Phase 5 |
| W4 | Phase 6 |
| W5 | Phase 7 |
| W6 | Phase 8 (power-loss validation: Phase 12) |
| W7, Phase 1 (Weft-Bench), Phase 2 SDKs, Phase 3, Phase 5 | Phase 9 |
| W8, 4.3 tag pruning, 4.6 | Phase 10 (open items: Phase 12) |
| W9 (shipped lanes), W10, B-tags, 0.3.x fixes | Phase 11 |
| W9 (open lanes), W6/W8 leftovers | Phase 12 |
| W11 | Phases 13–15 |
| W12 (Phase 7 acceptance) | Phase 16 |
| W13 (freeze) | Phases 16–18 |
| W14 | Phases 19–20 |
| W15 (1.0.0) | Phases 20–21 |
| Post-1.0: 7.1, S17, predecessor connectors, Prometheus remote write | Phases 22–23 |
| Phase 8 (commercial hardening), backup residues, Turso features | Phase 24 |
| Immediate next actions: reduction, sketch and benchmark research | Phase 25 |
| Phase 6 (Compression v2) | Phases 26–27 |
| Phase 9 (Analytics premium) | Phase 28 |
| SQL surface, Flight SQL, ML/AI | Phases 29–30 |

## Honesty pages

- Ship honesty pages

## Phase notes (old numbering 0–9)

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

- Python SDK → Rust SDK → Arrow Flight / Flight SQL → SQL surface / DataFusion (later)
  - **Python SDK** (2026-11-03): `clients/python` (package `weftdb`), standard-library only with `pyarrow` as the optional `arrow` extra. It covers every route: compute, catalog, the four ingest formats with `Idempotency-Key`, stored reads with cursor-following iterators, maintenance, backup and the bench runs. Storage values travel as `Decimal`/text and a `float` is refused at that boundary. The unit tests run against an in-process fake; `tests/test_live.py` is the contract test against a real `weft-server`, which CI runs on Linux after the build.
  - **Rust SDK** (2026-11-04): `weft-client`, async on `reqwest`, covering the same routes. Stored values are written from any `StoredValue` (decimal text, `BigDecimal`, integers) and read back as `BigDecimal`; `f64` does not implement `StoredValue`, so the float boundary is a compile error rather than a runtime refusal. Cursor-following reads are `Stream`s, refusals are one `Error` carrying the whole envelope, and an overloaded read is retried after `Retry-After`. Features: `rustls` (ring provider, platform or supplied roots) and `arrow` (Arrow IPC and Parquet to `RecordBatch`). `tests/fake.rs` pins the client's own behaviour against a fake server; `tests/live.rs` is the contract test against a real `weft-server`, which CI runs on Linux after the build.
  - **Arrow Flight** (2026-11-04): an opt-in, read-only gRPC listener (`WEFT_FLIGHT_ADDR`, experimental, loopback only since it is plaintext). `DoGet` takes a JSON ticket (`aspect` with `start`/`end`, or `lo`/`hi`) and streams the same typed record batch the Arrow range routes encode; `GetFlightInfo`, `GetSchema` and `ListFlights` describe the declared aspects. Keys and the `read` scope apply as on HTTP, denials are audited, a query gate sheds load as `UNAVAILABLE`, and refusals keep their envelope code in `x-weft-error-code`. `weft-server/tests/flight.rs` drives it with the Arrow project's `FlightClient`. Writes stay on HTTP.
  - **Flight SQL**: waits for the SQL surface (DataFusion, later): Flight SQL is SQL statements over Flight, and there is no SQL engine to answer them yet.
- Grafana → Prometheus remote write/read (if monitoring) → R/Arrow workflows
  - **Grafana** (2026-11-04): `GET /api/v1/grafana`, `POST …/metrics` and `POST …/query` answer the community JSON datasource (`simpod-json-datasource`), experimental and in the `read` scope. A panel's targets are aspects; each is the stored downsample over the panel's range at its interval (capped at `maxDataPoints`), the reduction picked per target (`agg`, default `avg`). `packaging/grafana/weftdb-datasource.yaml` provisions it; the unit test in `weft-server/src/grafana.rs` charts a stored aspect through the router.
  - **Prometheus remote write/read**: a remote-write series is a metric plus a label set (DECISIONS 2026-11-04).
    *Unblocked 2026-11-05: B-tags landed (TAG-5..TAG-11, #552-#570), so a label set can map to tags on one aspect. Not started: there is no remote-write/read route yet.*
  - **R/Arrow workflows** (2026-11-04): `clients/r` (package `weftdb`) reads stored windows, value bands and server-side downsamples as Arrow tables, pages the exact JSON rows, and writes data frames or Arrow tables as one Parquet batch with `Idempotency-Key`. Refusals are `weft_<code>` conditions. `tests/testthat/test-live.R` is the contract against a running server. It found that Parquet ingest refused Snappy, the default codec of R's arrow, pyarrow, pandas and Spark; the workspace's `parquet` now reads it.

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

- **6.1 Lossless typed codecs**
  - **Codec patent/licence gate (owner decisions 2026-10-08).**
    - **Default FOR codec and the Tiger Data family: with counsel.**
    - **The 2026-10-08 routine's advisory codecs: not yet reviewed, and ungated.**
    - **SAP family: review before the Phase 6.2 design.**
    - **Possible future: a patent-clear codec in a new crate.**
    - **Package licence metadata: mitigated by the relicense; the expression is with
  - **Evaluate Pcodec as an integer/f64 codec + bench baseline — now with the upstream numbers
    - **SCOPE CORRECTION (research 2026-09-23): evaluate pco for the COLD TIER and as a
    - **The real prize in pco may be its MODE DETECTION, not its format.**
    - **DONE (2026-10-08, advisory) — factor a timestamp column's common multiple (pco's `IntMult`) before DoD
      - **NEXT — realize the common-multiple factor as a timestamp codec flag (owner sign-off:
  - **f64-codec adopt: target ALP, not Chimp128/Elf — the state of the art.**
  - **ALP now has a FROZEN wire layout to mirror — do not invent a container (research
  - **DONE (2026-10-08) — Use the `alp` crate to get the ALP adopt benchmarked, rather than hand-rolling it first.**
    - **ALP adopt as `VAL_CODEC_ALP` — owner sign-off (headline change).**
    - **NEXT — realize the decimal-exponent FOR as `VAL_CODEC_DFOR` (owner sign-off: headline
    - **Re-decide the transposed layout now that the linear baseline is honest — RECOMMEND
    - **Next read-path floor: every point read still CRCs the whole frame** (real-data evidence
  - **Set the ALP acceptance bar from upstream's own ablation, and be willing to DECLINE.**
  - **GPU-decode ALP — retarget to G-ALP (DaMoN'25), and decide the exception layout BEFORE the
  - **Fuse decompression INTO the GPU interpolation kernel — with a number behind the wedge.**
  - **Evaluate Vortex as a columnar interchange + benchmark reference (Phase 1/4):**
- **6.2 Model-based compression** (leverages WeftDB's spline DNA, NeaTS-like)
- **6.3 Late/compressed-domain execution**
- **6.4 Quality benchmarks**

### Phase 7 — Online perf, durability, correctness · *High for beta*

Durability slices S1 to S9 and S18 have landed: legacy MVCC logs preserved and no-op sync pragmas dropped, a durable I/O layer with fault points and a power-cut simulator, hardened store open, store-wide sweep isolation, atomic publish of backup directories with a manifest and retention, schema v2 with `IndexTxn` and write poison, a persistent per-aspect id allocator with per-aspect commit and maintenance locks, both write-once maintenance slices, and legacy rows-mode hygiene.

Backups are online: control-plane snapshots over `VACUUM INTO` with per-database verification, a background daemon (`WEFT_BACKUP_INTERVAL_SECS`) that prunes generated snapshots (`WEFT_BACKUP_KEEP`), and a verify mode that is safe under concurrent writes. Restore and a backup/restore drill exist, and the drill is exposed at the API.

- **7.1 Online ingest**
- **7.2 WAL & crash consistency.**
  - **S10** Write-once durable seal with id assigned at commit *(~8 h; after S9)*
  - **S11** Rollup into segment_index.db, folded in the seal and swap transactions *(~7 h; after S10)*
  - **S12** Startup recovery, quarantine with TTL, fsck endpoint, /ready report *(~8 h; after S11)*
  - **S13** Idempotent atomic ingest entry point (library) *(~7 h; after S12)*
  - **S14** HTTP ingest wiring
  - **S15** Partial sidecar v4
  - **S16** Whole-store backup with hard-linked frames
  - **S17** Relaxed durability mode with synced_epoch watermark (optional for 1.0) *(~6 h; after S12)*
  - **S19** Seal-backed legacy aspects
  - **S20** Change log replaces the per-timestamp unbatched queue for seal-backed aspects *(~7 h; after S19)*
  - **S21** Crash-matrix CI, subprocess SIGKILL soak, durability benchmark and ROADMAP re-baseline *(~7 h; after S14, S16)*
- **7.3 Corruption detection**
  - Restoring *into a chosen new root* over HTTP
    *Landed 2026-11-14 (#213): `POST /api/v1/storage/restore/into?label=...&name=...` restores a whole store into `WEFT_RESTORE_BASE_DIR/<name>`. The offline drill at `weft-server/src/restore.rs:65` is unchanged — the HTTP route delegates to the same `restore_store` library call.*
  - Whole-store backup manifest (fold the `.weftseg` frames in beside the control plane)
    *Closed 2026-11-05 (audit): done 2026-11-01 (D-S16, #483). The `weftdb-whole-store-backup/1` manifest (`weftdb/src/types/backup.rs:412`) lists the hard-linked `.weftseg` frames beside the four control-plane databases; `restore_store` and `weft-server restore` read it.*
  - Document RPO/RTO against the measured snapshot cadence
    *Closed 2026-11-05 (audit): `docs/RPO-RTO.md` (EVI-19, #545); the soak backup lane records RPO/RTO by those formulas (SOA-11, #578).*
- **7.5 Compaction**
- **7.6 Quotas/limits**

### Phase 8 — Commercial hardening · *Required for paid beta*

The licensing decisions are made. Every WeftDB crate is `MIT OR Apache-2.0`, Justin Icenhour is the sole copyright holder, kdb+/KDB-X is excluded from published benchmarks unless KX consents in writing, and the flagged opt-in codecs are gated until counsel signs off.

- Security
- Compliance readiness
- Packaging
- Observability
- SDKs — Rust → Python → TypeScript → R/Arrow
- Licensing/legal
  - **Contribution terms: DCO or CLA, the contributor's choice.**
    - **Enforce it (owner, once on main):**
  - **With counsel:**
  - Still open

### Phase 9 — Analytics premium · *Medium, after benchmark foundation*

- Sliding windows → overlapping windows → event-centered windows
- Resample-before-compare
- Pattern dedup → occurrence-distance constraints
- Correlation / signal benchmarking
- Per-stage audit trail
- *(Future)* ML/AI

## 1.0 release criteria

### Durability & correctness *(the beta bar — Phase 7)*

- **7.2 WAL & crash consistency**
- **7.3 Corruption detection**
  *Closed 2026-11-20 (audit): every component named by the roadmap line is on main. D-S12 (2026-10-29, `DECISIONS.md:3570`) gave startup recovery, quarantine with a TTL (`weftdb/src/types/quarantine.rs`) and the fsck route (`weft-server/src/routes.rs:967`); ROB-8 (#265), ROB-9 (#315) and ROB-22 (#446 frame-fault policy verification, #720 suspect-frame policy and background-decode crash-loop breaker) are closed; restore into a chosen new root over HTTP landed 2026-11-14 (#213). The offline drill at `weft-server/src/restore.rs:65` is unchanged.*
- **7.4 Whole-store backup manifest**
  *Closed 2026-11-05 (audit): whole-store manifest `weftdb-whole-store-backup/1` shipped in D-S16 (#483).*
- **7.4 Documented RPO/RTO** against the measured snapshot cadence
- **7.5 Compaction** with query consistency, cancellation, and resource limits
- **7.6 Quotas/limits**
- **Phase 7 acceptance run**

### Hot path & data model *(must land before the freeze)*

`splimes` 1.0 is released and WeftDB depends on it, and the control-plane engine decision is made.

- **Measurement bulk ingest routed through the `.weftseg` seal**
  *Audit 2026-11-16 (#232, part 1): the two legacy bulk writers that do not read the stored
  range first — `capture_new_measurement` and `batch_capture_new_measurements` in
  `weftdb/src/types/database/inputs.rs` — now route through the seal store for a seal-backed
  aspect, the way the D-S19 paths already did. Before this they wrote the per-aspect rows-mode
  `measurements.db`, which no seal-backed read consults, so the write was invisible: the count
  answered 0 after a batch that had returned `Ok`. Rows mode is unchanged, and
  `capture_new_measurement_chunk` is now documented rows-mode-only because its caller supplies
  the rows-mode handle. The line stays unticked: new aspects are still created in rows mode
  (`StorageMode::for_new_aspect`, `weftdb/src/types/legacy_seal.rs:126-135`), and flipping that
  default is **#768**, which carries the ingest/read cost figure the flip needs.*

  *Audit 2026-11-16, #768 (the flip): it shipped, and the figure named above is not a
  gate at all. `float_roundtrip` prices the JSON body of an ingest or interpolate request, not
  where a new aspect puts its rows, and the seal path's real costs, one-row ingest as a full
  seal commit, the fragmentation gate, read fan-out, were already characterised in S19 and S20.
  What the flip does need stating is the one-way door: an unset variable now leaves a new aspect
  seal-backed, and 0.3.7 or earlier read zero measurements from such an aspect instead of
  refusing it. The gate was the downgrade note in `docs/upgrade-notes.md`, not a benchmark.*
- **B-tags**

### Security & packaging *(Phase 8 minimum)*

- TLS on `weft-server`
  - *Audit 2026-11-04 (SEC-13): checked — the acceptance suite (`weft-server/tests/security_acceptance.rs`) boots the real binary with a TLS certificate and a mounted keys file and serves the whole secured surface over HTTPS, with plaintext to the same port unserved.*
- API-key/token auth + basic RBAC + audit log
  - *Audit 2026-11-04 (SEC-13): checked — the same suite pins the full role × scope matrix (SEC-3's six roles × eight scopes, enforced by SEC-6's keys), live key reload, and SEC-9's audit trail for a mixed run of denials and mutations, in-process and over the wire.*
- Packaging
- Upgrade/rollback procedure, exercised against the previous release

### Release engineering

Dependencies were brought current and audited on 2026-10-05, `SECURITY.md` is on main, and `cargo deny check advisories bans licenses sources` runs in CI.

- **CI**
- **Public surface declared and frozen**
- **Compatibility tests**
- **Semver policy + `CHANGELOG.md`**, starting from the first tagged pre-release
  *Closed 2026-11-20 (audit): 11 tags, 11 changelog sections, 11 compare links, baseline v0.1.0 2026-10-08. Pre-release policy at `docs/stability.md:47`.*
- **Crate publishing split**
- **Panic audit on server paths**
- **Security process**
  - **Private vulnerability reporting is off**
    - *Audit 2026-10-27: checked — `gh api repos/basic-automation/weftdb/private-vulnerability-reporting` now returns `{"enabled":true}` (the heading text is historical). The other children hold: SECURITY.md routes reports to GitHub private reporting, and the `deny` job in ci.yml runs advisories, bans, licenses and sources.*

### Claims & documentation

- [x] Every performance claim in the README links to a benchmark artifact (the [governing rule](#weftdb-roadmap))
  — ticked by [CLAIMS-243](https://github.com/basic-automation/weftdb/pull/832): the README has no bare numeric figures; its qualitative claims point at the committed pooled report at `bench/reports/phase1-pooled/`. Registry entry `cross-engine-2026-11-02` added.
- First cross-engine report
- Honesty pages shipped (see [Commercial thesis](https://basicautomation.io/projects/weftdb/docs/commercial-thesis))
- Legal review of competitor benchmark-publication terms + codec licenses (see [Legal review](#legal-review))
- README *Project status* table updated to reflect 1.0 and the status badge changed from `pre-beta`

### Execution waves (W0–W15)

Later waves landed the v0.1.0 baseline release (PAC-4, a GitHub release only) with one fixture set written by that binary (X-FX), and the stale draft prerelease was deleted. The server and store lanes followed: the `manage.rs` and `main.rs` split (X-SPLIT-SRV), write-once maintenance II (D-S9), the configuration registry skeleton (PAC-5a), the single error envelope and code table (X-ERR), `aspect_name::encode` and `validate_new` at declare (X-NAME), storage-engine panic fail-stop (ROB-2), and the frame registry with its v7 envelope (X-FRAME).

On packaging, PAC-1b added the stripped dist profile with a third-party licence bundle, and PAC-3 stopped the library crates imposing a global allocator or wgpu features on their users. The remaining waves and open lanes are listed below.

- **W0 · Land in-flight work and lock the shapes S6/S7 depend on**

  In-flight work is merged: INT-0 landed the splimes-1, legal and durability branches into main. The store lane's shapes are locked and implemented: Storage-v2 open hardening (D-S3), atomic backup publish with manifest, retention and drill hygiene plus the backup-label grammar (D-S5, SEC-1), legacy rows-mode hygiene (D-S18), schema v2 with `IndexTxn`, write poison and root-relative frame paths (D-S6), and the persistent per-aspect id allocator with commit and maintenance locks and plain INSERT for seals (D-S7). Decision 1 records the store shapes S6 and S7 rely on.

  On the release and identity side, the release pipeline (PAC-1a) and the canonical `TagSet`, `SeriesKey` and `SeriesSelector` types with their frozen caps (TAG-1) are in, and GitHub private vulnerability reporting is turned on (owner step).

- **W1 · Baseline release, server split, goldens**

  Release truth is settled: per-crate versions, a single edition, the publish split and package hygiene (FRE-1). Golden frames, sidecars and persisted-encoding goldens, gated codecs included, are committed (FRE-3), and weftdb's `ValueKind` is open with a provenance scope (TAG-3). The README's commands, flags, endpoints and links were checked against the code on 2026-10-25 and its one over-claim reworded (EVI-1). Bench runs carry a manifest at schema v23 (EVI-3). `weft-bench --sparse` measures a seeded nullable grid and records the observed missingness, and `--scale tiny|small|medium|large` sizes the sparse, gap-fill and downsample workloads and names the preset in the profile (EVI-9). The soak harness skeleton is in (SOA-1).

- **W2 · Error contract, frame format, portable builds**

  The legacy lane closed FRE-14c: `weft-server import-rows-mode` streams one 0.1.x subject into a Storage v2 store, migrates `metadata.db`'s non-row state, reports what stays behind and refuses a second run. Linux binaries are portable on a glibc 2.28 baseline (PAC-2). A nightly regression workflow and `weft-bench compare` watch performance (EVI-5). `bench/claims.json` vouches for every README figure and `tools/check_claims.py` enforces the markers, sources and links in CI (EVI-2), with the artifact pipeline behind it (EVI-4). The ingest workload over SegmentStore with the UCI loader (EVI-6) and benchmark adapter v2 I (EVI-8A) landed. On the soak side: preflight, device resolution, run metadata and secret redaction (SOA-2); the open-loop scheduler, HDR interval logs, self-monitoring and null server (SOA-3); and the deterministic workload model with format-exact value grids and body encoders (SOA-4).

- **W3 · Gate, serve path, input domain**

  The store lane added library stored-range interpolation with input caps (EVI-10A) and the rollup in `segment_index.db`, folded into the seal and swap transactions (D-S11). Routes live in a sealed registry with a frozen scope table and a fail-closed gate (SEC-3), served by one HTTP/1.1 path with pre-auth listener limits that absorb ROB-23's caps (SEC-4). Decimals are bounded (ROB-6). Benchmark adapter v2 II (EVI-8B), the soak ingest client (SOA-5) and the Linux system samplers with contamination flags and the strict-mode flush check (SOA-12) round it out.
- **W4 · Auth, config, ILP fix**

  The store gained an idempotent atomic ingest entry point with `groups`, ledger `series_ids` and typed errors (D-S13). The server got a rustls/ring TLS listener on the shared serve path (SEC-5), API keys with Bearer auth, a keys file and keygen (SEC-6), panic containment with daemon supervision, the Daemons registry and `weftdb::exec` (ROB-3), and strict configuration with a store filesystem check (PAC-5b; the bind guard moved to SEC-8). ILP series merging is fixed (TAG-0) and frame decoding hardened (ROB-8). The stored WeftDB benchmark adapter (EVI-11) and the soak oracle core (SOA-6) landed.
- **W5 · Ingest wiring, whole-store backup, legacy seal**

  The store lane added the partial sidecar v4 (D-S15) and seal-backed legacy aspects (D-S19), and L-INGEST wired ingest through. The no-panic lint, asserts included, is enforced and weft-arrow exposes a `Result` API (ROB-4); admission I is in (ROB-10). Startup ordering, the exposure matrix, the auth-off guard and metrics auth (SEC-8) shipped with exposure and secrets hygiene (SEC-10). The compute vocabulary uses exact-case tokens with no silent narrowing (FRE-5), and declarations are immutable with scale rules and schema read-back (FRE-7b). DuckDB adapter I (EVI-13), the report renderer with generated honesty sections (EVI-15R) and honesty pages I (EVI-16) landed, as did soak window digests with an oracle self-test (SOA-7) and the failure deck with phases, recovery windows, the SIGTERM verdict and the maintenance driver (SOA-8).

- **W6 · Durability CI, store split, scheduler**
  - **D-S21** Crash-matrix CI, subprocess SIGKILL soak, durability benchmark and re-baseline, with **SOA-9** (`crash_kill9`)
  - Run D-S21's privileged power-loss validation (sudo: dm-log-writes or a QEMU hard reset) *(owner)*
  - Store: **X-SPLIT-STORE** Split segment_store.rs into `open`/`read`/`seal`/`maint`/`stats` submodules
  - → **TAG-4** weftdb read sites on the frame-reader API with typed errors
  - **FRE-6b** Strict request parsing and caller updates
  - **SEC-7** Key reload
  - **SEC-9** Audit
  - **SEC-11** Aspect-name traversal HTTP regression tests
  - **PAC-7** Operator CLI, exit-code contract and store identity at startup
  - → **ROB-11** Admission II
  - **ROB-19** Maintenance scheduler, compaction on by default, per-aspect circuit breaker
  - **SOA-10** Derived-read checks
  - **SOA-14** Server hooks

  - **EVI-14** DuckDB adapter II
  - **EVI-18** Backup health metrics and catch-up-free ticking
  - **EVI-25** Backup, restore and recovery cost measurement (RTO inputs and backup duration)
- **W7 · Post-split lanes open; security and packaging close**

  The open lane pinned the store scope: a start whose `WEFT_STORE_DATABASE`/`WEFT_STORE_SUBJECT` differ from the root's `STORE_FORMAT` exits 65 unless `WEFT_STORE_SCOPE_PIN=off`, and `GET /api/v1/version` answers build, layout, scope-pin and exit-code state from process state alone (FRE-12b; DECISIONS 2026-11-03). The read lane bounded stored downsample and point lookups (ROB-13). Ingest rejects non-finite input as `400 not_finite` in every format and enforces `WEFT_MAX_INGEST_ROWS` and `WEFT_MAX_INGEST_DECODED_BYTES` before the seal (ROB-7, #551). GPU configuration, compute limits, interpolation provenance and metrics (ROB-16), the probe-only admin listener (ROB-23r) and the legacy queue-consumer panic fix (ROB-5) landed. Security acceptance closed (SEC-13). Packaging shipped the offline `weft-server restore` CLI (PAC-13), the Docker image with a container smoke test (PAC-9), and the systemd unit with host-install CI (PAC-11). The first cross-engine report and its owner-approved narrative (EVI-15), documented RPO/RTO (EVI-19) and the soak backup lane with final restore verification (SOA-11) close the wave.
- **W8 · Tags core, memory budget**
  - open lane
  - → **TAG-9** Frame error classes in recovery
    *Closed 2026-11-05 (audit): #556 (2026-11-04). Recovery and fsck adopt series-key frames into their series, class a malformed/empty/duplicate SERIES_KEY after a passing CRC as suspect (C5), rebind mismatched keys (`series_rebound`), and settle every held row (`held_cleared`).*
  - → **TAG-5** Tags core: series table, catalog, and the layout marker
    *Closed 2026-11-07 by audit (#619): the series catalog and its time index are registered
    migrations `0006_tag5_series` and `0007_series_time_index`, and the rest of the C1 columns
    landed earlier in the same registry (`0002_s6_s7`, `0004_d_s12`, `0005_d_s13`). No
    `0009_*` and no bump: `SUPPORTED_LAYOUT` stays 2 and layout 3 stays unassigned, because every
    shape of a marker-only migration either makes 0.3.x binaries refuse a store that differs only
    by a re-run of `CREATE TABLE IF NOT EXISTS series` or teaches that a layout number can move
    without meaning anything. DECISIONS 2026-11-07; the owner still owes the ruling on whether the
    literal `layout_version = 3` is a hard 1.0 gate.*
  - seal lane *Closed: TAG-7, #554.*
  - → **ROB-17** Disk guard, headroom API and space gauges
    - Log the `507 insufficient_storage` refusal once per transition into and out of refusing, not once per request: today every refused ingest logs at ERROR, so a client retrying against a full disk floods the log (the `weft_disk_guard_refusing` gauge already carries the state).
    - Report the guard's state on `/ready` (a `disk_refusing` field, still `200`) so a balancer or ingest router can steer writes elsewhere while reads keep being served — VictoriaMetrics' `vmstorage` does exactly this: it switches to read-only below `-storage.minFreeDiskSpaceBytes` and `vminsert` re-routes data to the remaining nodes (https://github.com/VictoriaMetrics/VictoriaMetrics/blob/master/docs/victoriametrics/Cluster-VictoriaMetrics.md#readonly-mode).
    - ROB-18 phase 1: call `weftdb::DiskGuard::admit` with the reserved body size, so a request is refused `507` before its body is read (plan C11; the guard is public for it). *(#681)*
    *Closed 2026-11-14 (#721): contract tests pin the five admission behaviours over HTTP (413 declared-over-cap and 507 reserve-before-body-read) and per-phase unit tests cover 408 body timeout, 507 decoded top-up, and the two-code panic split.*
  - maint lane
  - read lane
  - **ROB-14** Cgroup-aware memory budget with per-request reservations
  - **FRE-8b** Stored-value text and one float-input rule across every ingest format
  - **FRE-9a** OpenAPI generated from the route table, committed, drift-checked in CI
    *Closed 2026-11-07 (#615): generated from the real `RouteTable` via
    `openapi::from_table`, wired as the `weft-server openapi` subcommand
    (PAC-7 exit-code contract), committed at `compat/openapi/weftdb.json`,
    drift-gated by `generated_spec_matches_committed_file`, and structurally
    validated by `generated_spec_passes_structural_validation`.*
  - **ROB-9** Mutation harness for frames, sidecars and Parquet
    - Report the three `parquet` 60.0.0 decoder panics upstream to apache/arrow-rs (`offset + len out of bounds`, `column start and length should not be negative`, `Decoder for dict should have been set`; reproducers are one-byte edits found by `weft-arrow-store/tests/parquet_mutation.rs`), plus an `arrow-buffer` 60.0.0 panic at `util/bit_chunk_iterator.rs:235` seen when 4,070 one-bit mutants were posted to a running server *(owner: outward-facing)*. WeftDB contains them in `weft_arrow::read_parquet` meanwhile (DECISIONS 2026-11-05). Before filing, check them against the two malformed-file panics upstream already closed in 2026 — a compact-Thrift bool `unwrap` (https://github.com/apache/arrow-rs/issues/9839, closed 2026-04-28) and a row-count/column-data mismatch index panic (https://github.com/apache/arrow-rs/issues/9992, closed 2026-05-22) — since ours were seen on 60.0.0 and are different messages.
  - **SEC-14** Security documentation, config schema and upgrade notes
    *Closed 2026-11-09: `docs/security.md` rewritten with source citations, full role table,
    credential-in-query guard, proxy guard, exit status table, startup enforcement, and
    known limitations. `docs/upgrade-notes.md` already existed; `docs/config-schema.md`
    updated by a concurrent PR.
  - **PAC-10** Publish the multi-arch image (only after API auth)
  - Make the ghcr.io weftdb package public after PAC-10's first push *(owner)*
  - **EVI-17** Honesty pages II
  - Write or approve the honesty-page narrative *(owner)*
  - **FRE-13** Layout-1 to layout-2 upgrade test
    *Closed 2026-11-09 (#666): `weftdb/tests/layout_upgrade.rs` covers the [layout 1 → 2 upgrade contract](https://github.com/basic-automation/weftdb/pull/666) through the public API.*
  - **SOA-13** SoakReport, SLO evaluator R1–R8, coverage floors, honesty section
    *Closed 2026-11-07 (#603): `SoakReport`, the R1–R8 evaluator, the coverage floors and the
    honesty section are on main; the report is what #616's nightly comparison reads.*
  - → **SOA-15** soak.yml nightly workflow, TTL lane and nightly-to-nightly comparison
    *Updated 2026-11-09: parts 1 (nightly workflow with active cron `0 7 * * *`) and 3
    (nightly-to-nightly comparison with first-night-no-baseline path) shipped in #616 and
    verified present on `origin/main`. Part 2 (TTL lane) is active on this branch —
    `WEFT_ROW_TTL` reaches `SegmentStoreOptions::from_env` (`#784` on main), and the soak
    drives a store-side TTL lane (`weft-bench/src/soak_ttl.rs`) that exercises
    `read_time_range_of_with_expiry`, `expiry_sweep` and `purge_expired`. The honesty
    section renders the verdict. The lane opens its own temporary store (the soak uses
    `NullServer`, so no server endpoint is exercised). It proves the expiry-aware read
    filter, the sweep and purge plumbing, and that a `NoExpiry` store reports 0 expired
    rows while a TTL store with aged rows reports >0. Still not covered: HTTP-level
    expiry, cross-restart TTL persistence, and concurrent expiry with other
    maintenance operations.*
- **W9 · Read semantics, limits, upgrade legs**
  - read lane
  - maint lane
  - open lane
  - L-INGEST
    ROB-18 closed (#721): #681 shipped the phase 1 disk-guard admit and the deadline/decoded-top-up/panic-mapping wiring; contract coverage pins it.
  - storage read handlers lane (TAG-10)
  - **FRE-10a** Contract tests
  - **PAC-14a** UPGRADING.md and compat legs A (upgrade) and C (rollback after writes) (absorbs FRE-19)
  - **SOA-16** Soak upgrade lane
- **W10 · Tag gate flip, compaction closes**
  - **TAG-11** HTTP ingest with tags and the tagged-write gate flip
  - **ROB-21** Maintenance cancellation, RAII cleanup guards, minimal maintenance metrics
  - read lane
  - **FRE-15** STABILITY.md, semver policy and the release-criteria wording
    *Closed 2026-11-08: [`docs/stability.md`](docs/stability.md) documents the semver
    policy, surface stability table, compat-suite coverage and named gaps; linked from
    README.md and docs/reference.md.*
  - **PAC-15** Operator documentation
- **W11 · Contract candidate, beta tag, calibration**
  - Move the T7 reference drive (`/mnt/weftbench`) to a 10 Gbps port before calibration *(owner)*
  - **FRE-9b** OpenAPI
    - Extend the release tag grammar to `vX.Y.Z-beta.N` (`TAG_RE` in `scripts/release/verify.sh` accepts only `vX.Y.Z` and `vX.Y.Z-rc.N` today, so a beta …
    - Approve and push the **v1.0.0-beta.1** tag on FRE-9b's merge *(owner)*
  - → **SOA-17** Calibration on a pre-grown store, frozen acceptance profiles, `scripts/crash-soak.sh`
  - **FRE-10b** Contract tests
  - → **FRE-16** CI compatibility gates
  - **TAG-12** Multi-series crash cell landed (issue #341); the fan-out arm landed in #809; TSBS benchmarks, fixtures, freeze docs still outstanding
  - **ROB-24** Limits and overload evidence
- **W12 · Phase 7 acceptance**
  - **SOA-18** 24 h acceptance run on the reference SSD, published as an artifact
  - Run the privileged power-loss validation on the acceptance commit *(owner)*
- **W13 · Freeze**
  - Settle the [Phase 8](#phase-8--commercial-hardening--required-for-paid-beta) counsel items (codec patents, **WEFTDB** trademark, benchmark policy) before rc.1 *(owner)*
  - **FRE-18** rc.1 freeze cut and layout-2 baseline fixtures
    - Approve and push the **v1.0.0-rc.1** tag on FRE-18's merge *(owner)*
- **W14 · RC verification on release artifacts**
  - **SOA-19** RC soak on the rc release artifact in the shipped default config (8 h, or 24 h)
  - **EVI-21** 1.0 evidence run at the rc
  - **PAC-18** Dress rehearsal
- **W15 · 1.0.0**
  - Write or approve the 1.0 release notes and upgrade notes before the cut *(owner)*
  - **ROADMAP-236** The upgrade/rollback line was split (issue #236, PR #833). The upgrade half is exercised by the `compat` job's upgrade leg against the only pinned release, `v0.1.0` — the sole release whose store needs a layout migration. The rollback half was dropped from the line because FRE-16 (2026-11-10) declared rollback-after-writes unsupported for 1.0; only the `SUBSTITUTE_RUNNERS` stand-in covers it, which is explicitly not the leg.
  - **EVI-22** README 1.0 project status, badge and versions
  - → **PAC-18** Cut 1.0.0
    - Approve and push the **v1.0.0** tag *(owner)*

## Predecessor-derived backlog

The interpolation item (B-interp, Phase 5) is done.

- **B-tags** *(Phase 2/4 · done: TAG-5..TAG-11)*
- **B-conn** *(Phase 2/7 · absent)*
- **B-ilp** *(Phase 2 · partial)*
- **B-rest** *(Phase 2 · partial)*
- **B-poll / B-retry / B-register** *(Phase 7 · absent)*
- **B-analysis** *(Phase 4/9 · verify)*
- **B-object** *(Phase 4 · absent)*
- **B-windows** *(Phase 9 · absent/partial)*
- **B-dedup** *(Phase 7 · partial)*
- **B-sim** *(Phase 9 · partial)*
- **B-precision** *(Phase 4/6 · partial)*
  - 2026-11-20: BigDecimal precision-boundary inventory produced (`docs/research/bigdecimal-inventory.md`) (#840)
- **B-resilience** *(Phase 4/7 · partial)*
- **B-conncache** *(Phase 4/7 · partial)*
- **B-msgpack** *(Phase 6 · absent)*
- **B-snapshot** *(Phase 3/9 · absent)*

## Control-plane engine: Turso/libSQL 0.6 adoption

Turso is migrated from 0.4 to 0.6 and then to 0.8.1 (2026-10-05), with the control-plane schemas moved to plain `INTEGER PRIMARY KEY` for MVCC. The 0.7 upgrade evaluation is settled. Online backups use `VACUUM INTO` with a concurrent-write-safe verify and a background daemon, and snapshot directories no longer accumulate empty `.db-log` and `.db-wal` sidecars. `Statement::n_change()` affected-row accounting is shipped for the aspect-metadata write and the seal's index write (Phase 3).

The backup cost was measured on 2026-09-23, and the diagnosis on record at the time turned out to be wrong; the corrected one is in the docs. Everything still listed below is control-plane-only by design: none of it turns Turso into the measurement backend.

- Lean on production MVCC concurrent writes for concurrent ingest + catalog updates under load *(Phase 7)*
- Encryption at rest (AEAD pager) for catalog/metadata/pipeline-state DBs *(Phase 8)*
- **Backup 7.4 residue (b) — incremental / WAL-streaming backup** beyond the full `VACUUM INTO`
- **Backup 7.4 residue (d) — whole-store backup manifest**
  *Closed 2026-11-05 (audit): shipped in D-S16 (#483) with hard-linked frames (`weftdb/src/types/backup.rs:412`).*
- **Backup cost residue (a) — the root cause is documented upstream, and it is per-call
- **Backup cost residue (b) — (i) MEASURED 2026-10-08: `synchronous=OFF` is NOT a material win.**
- **Backup cost residue (c) — DO NOT reach for Turso's experimental passive checkpoint.**
- **Backup 7.4 residue (b) is UPSTREAM-GATED, not an open design question (research 2026-09-23):**
- **Backup cost residue (d) — evaluate an `sqlite3_rsync`-style page-delta replica.**
- **Confirm the snapshot verifier does not depend on MVCC-broken pragmas.**
  - *Audit 2026-10-27: checked — `the_verifier_never_relies_on_pragmas_mvcc_reports_as_zero` (weftdb/src/types/backup.rs:1250) statically asserts no verifier code names `page_count`/`user_version`/`application_id`/`freelist_count` and behaviourally checks both `VerifyMode`s count real tables/rows on an MVCC source.*
- Triggers (`BEFORE/AFTER/INSTEAD OF` + `WHEN`)
- Dynamic auth tokens as closures
- `UPDATE … FROM`, aggregate `FILTER`, `INDEXED BY`, `NULLS FIRST/LAST`
- Evaluate (don't rush)

## Six-month execution plan

The bodies of these six lines were cut off in #99. The text below is restored from the version of the roadmap before that change.

- **Month 1 — Benchmark truth:** external-engine adapters (WeftDB + DuckDB minimum, then ClickHouse/InfluxDB 3/QuestDB/TimescaleDB); first cross-engine report (no aggressive claims); methodology + anti-Goodhart policy + hardware-reporting template + correctness rules published
  - Closed 2026-11-12. All of the adapters live in `weft-xbench/src` and were run in the pooled Phase 1 report (`bench/reports/phase1-pooled`, #508, 2026-11-02), which also includes IoTDB. The first cross-engine report is EVI-15 (`bench/reports/evi-15`). The methodology, hardware reporting and correctness rules are in `docs/methodology.md`, and the anti-Goodhart policy is in `HONESTY.md`.
- **Month 2 — Public API & observability:** OTel; Parquet bench output; cold/warm separation; GPU timing breakdown *(server, REST, ILP, Prometheus, p50/p95/p99 shipped)*
  - Closed 2026-11-12. `weft-server` exports OTel. Bench output goes to Parquet through `weft-bench/src/parquet_report.rs`. Cross-engine phases are marked `os-cold`, `restarted` or warm. The GPU benches (`gpu_economics`, `e2e_interpolation`) time device open and kernel compilation separately from the timed cells.
- **Months 3–4 — Hot-path optimization:** batch-ingest rewrite; minimized `BigDecimal` hot-loop use; typed-array interpolation streaming; nightly benchmark-regression CI. *Goal: establish whether storage or conversion is the dominant bottleneck.*
- **Months 4–5 — GPU flagship:** command batching; true async handles; CPU/GPU overlap; hardware auto-tuning; end-to-end interpolation benchmarks; CPU-only vs GPU cost/performance report; portability/determinism matrix. *Publish only if strong and reproducible.*
  - Closed 2026-11-12. Command batching (5.1) and async handles (5.2) were measured and dropped rather than built. The rest shipped:
    - CPU/GPU overlap: streamed interpolation (#585)
    - Hardware auto-tuning: startup calibration for `Backend::Auto`
    - End-to-end interpolation benchmarks: `weft-e2e-interp` (#586)
    - CPU vs GPU cost/performance report: `--gpu-economics` (#569)
    - Portability/determinism matrix: the conformance matrix (#573)
- **Month 5 — Compression v2 prototype:** scaled-int compression; f64 codec prototype; random-access blocks; model-based prototype; accuracy benchmarks
- **Month 6 — Commercial beta:** Docker image; API keys/TLS; backup/restore MVP; Python SDK; Grafana dashboard; benchmark report; customer-runnable Weft-Bench; design-partner onboarding docs. *Goal: a "WeftDB Performance Preview" for 3–5 partners.*

## Legal review

Three decisions are made and recorded: kdb+/KDB-X is excluded from every published benchmark unless KX consents in writing, WeftDB's own terms are `MIT OR Apache-2.0` with contributions by DCO sign-off or the CLA, and the flagged opt-in codecs stay off unless a build asks for them — with **no licence acknowledgement recorded anywhere**, which is the gap #386 exists to close. The counsel items are still open below.

- Review each competitor's benchmark-publication terms (kdb+ especially)
  - **Done 2026-11-15 as a terms inventory, not a legal opinion** — [docs/legal-review.md](legal-review.md)
    records what each vendor's terms say about publishing a benchmark, with a
    verification level stamped on every row (Read / Recorded / Inherited), and
    what WeftDB's own terms oblige us to do about reproducing our own published
    numbers. The premise it corrects: the free Kdb+ On Demand licence forbids
    running benchmarks *for publication* (§1.2) and publishing benchmark reports
    (§1.3) outright, so excluding kdb+ is a licence term and not a conservative
    reading of inherited claims. The commercial KDB On Demand clause binds *our
    customers* and never us, so it cannot be the source of an exclusion for our
    own runs. The KX Benchmarking Agreement page the roadmap notes cited is a 404;
    the consent instrument has to be found through KX sales before any kdb+
    number is published.
  - **Done 2026-11-15 as a draft policy** — [docs/benchmark-policy.md](benchmark-policy.md)
    states the rules in force for engineers: self-hosted open-source servers
    only, the replication set every published comparison must carry, the
    `NOT_IN_SCOPE` gate treated as a legal surface (changing an entry needs a
    citation with a fetch date in the same PR), kdb+ and AVEVA PI excluded until
    their vendors respond, and no competitor run under an evaluation or
    non-commercial licence without a recorded counsel sign-off. Marked **draft
    for counsel review**: it gives counsel something concrete to correct, and it
    settles none of the counsel items.
  - **Open with counsel:** the codec patents and licences (#386 and the ungated
    advisory codecs at `ROADMAP.md:138`), the missing `NOTICE` and
    third-party-terms file for the Elastic-licensed `aggs-for-timeseries` pin,
    DuckDB's trademark policy, a re-read of the live ClickHouse Acceptable Use
    Policy before the first public ClickHouse number, and the licence reason
    inside `NOT_IN_SCOPE` entry 5, which cites a `WILLINGNESS-TO-PAY.md` this
    repository does not contain.

## Immediate next actions

The decimal-versus-f64 gap was measured at about 43x rather than 2x, which set the course for the integer-native path: `weft_reduce::reduce_scaled` and the scaled partials, the `avg` division floor, per-bucket bookkeeping off the integer path, time-weighted averages on integers, exact percentiles by selection, and sketch improvements. All of it is wired into `SegmentStore::downsample_range`. The bench harness times the integer path too and runs the compression, point-lookup, range-fetch and downsample workloads on real corpora, and the storage generators were found to flatter, so they now carry realism knobs.

Around that, the read-path arc is done: streaming and batch point lookup (also at the API), closed-form and block-random-access timestamp search, and windowed range reads. The `weft-reduce` crate has mergeable DDSketch quantiles, cross-segment downsample at the store layer and the API (4.7x with concurrent segments), per-segment `.weftpart` partials with a hierarchy of materialised tiers, and size-targeted compaction with a fragmentation gate in the reconcile daemon. Test infrastructure is healthy: the flaky GPU tests, the rustc ICE and the link OOM were root-caused and fixed, `cargo test --workspace` completes (969 passed), and the BTC bulk-load test is runnable. The codec and reconciliation items from Phases 2 to 6 that were tracked here are summarised in those phases.

- **START HERE (filed 2026-11-11 for the next run).** The 2026-11-11 run stopped inside an hour, after three `weft-reduce` slices (the UDDSketch item below) because the host could not carry more: load average 90 to 120 on 16 cores, 4 to 5 GB of 124 GB available with a 76 GB model server resident, a `git status` that took over four minutes, and other sessions' `cargo` runs in flight. Builds ran inside `systemd-run --user --scope -p MemoryMax=6G` so an overrun would kill the build and nothing else; that worked and is worth keeping. `cargo test --workspace` was not run. Next, in order: (1) on a quiet host, the timing rerun filed under the UDDSketch item; (2) the real-filesystem ENOSPC test for #656, on top of the injectable space source once #694 merges; (3) "select on the mantissas when a partial is not merged" (#401), which is a pure speed item and needs the same quiet host. The two `weft-orchestration` test items below ("three unguarded … failures" and "Bound the seven slow … tests") are closed on GitHub (#402, #404) as not actionable as written; read those threads before picking either up.
- **README `## Status` is stale (noticed 2026-11-05, fixed 2026-11-10 by #682):** it said v0.2.1 (the release is 0.3.0) and lists tags and client SDKs as "Not started", but B-tags (TAG-5..TAG-11) and the Python, Rust and R clients have shipped. Owner-facing wording; EVI-22 rewrites the table at 1.0, but it is wrong today.
- **START HERE (rewritten 2026-11-11 for the next run).** The 2026-11-05 list below is kept as a record and is stale: every item it named has since been ticked, merged, or taken by the nightly routine loop. Check #125 and the open PRs for claims first. The unclaimed queue is: the dictionary occurrence footprint (`:485`), the memory-per-database-file RSS question (`:492`), the GPU-contention flake (`:456`), and the one measured figure still missing under the `float_roundtrip` entry (`:453`). Per-pipeline signal state (`:482`) is ticked below — it shipped in #673 and only the board forgot. The bounded-window detector entry (`:484`) is ticked below as merged by #692. The `weft-orchestration` slow-test items below it are now ticked, with why. `PAC-10` itself is ticked (#680); what is left is the owner item under it (`:322`), making the ghcr.io package public — that line needs a human, and a worker cannot even verify it: listing the org's packages answers `You need at least read:packages scope`. Builds: the shared `Cargo Target` lock was held for 30+ min by other projects' tests on 2026-11-05; the warm `/mnt/weftbench/target-ci` worked, and clippy needs `RUSTC_WRAPPER=` because the global nightly-threads wrapper breaks `clippy-driver`.
- **Two `weft-server` bit-exactness tests failed under `cargo test -p weft-server` (found and fixed 2026-11-05):** `interpolate::tests::ilp_values_feed_the_f64_path_unchanged` and `stored_interpolate::tests::a_multi_chunk_stream_is_the_library_result` were 1 ULP off because serde_json's default float parser is not correctly rounded, and only a whole-workspace build (where bigdecimal's `serde-json` feature turns on serde_json's `arbitrary_precision`) parsed correctly. The release workflow and the Docker image build `-p weft-server`, so the shipped binary read some client JSON floats 1 ULP off. Fixed by serde_json's `float_roundtrip` workspace-wide, pinned by `num::json_float_parse_tests`.
  - Measure what `float_roundtrip` costs the float parse itself: on 1,000,000 values of the compute routes' shape (magnitudes 1e-19..1e15, ryu-formatted, a 20.7 MB body), serde_json 1.0.151 release build pinned to one core of a Ryzen 9 7950X3D under load: 40.0–41.7 ns/value without the feature and 57.0–59.2 with it (≈ +17 ns, ≈ 1.42×, 8 runs each), against 175,103 → 0 values that failed to round-trip. The harness was a scratch crate; the correctness half is committed as `num::json_float_parse_tests::a_seeded_sample_of_json_floats_round_trips_exactly`.
  - What that costs a whole JSON compute or ingest request: a request-level benchmark (`weft-server/benches/json_body_cost.rs`) drives POST /api/v1/interpolate through the real axum router and reports p50/p95 + ns/value — the same metric at a different stack level. The number itself has **not been measured** yet: the A/B still has to complete on a hosted runner. Three dispatches of the first version died around the 11th minute with `The runner has received a shutdown signal` / exit 143 while still compiling, and the logs show why it could not make progress: the rust-cache restore took 0 s and its post step was skipped, so nothing was ever restored or saved (the action saves only on job success, and the job never succeeded), while `CARGO_BUILD_JOBS=2` doubled the wall time of that always-cold ~600-crate graph. `.github/workflows/float-roundtrip-ab.yml` now runs one arm per job on a matrix (`on`, `off`), keys the cache per arm with `save-always: true`, and leaves cargo's default job count alone, so a re-dispatch resumes warm. `values`/`reps` inputs are exported as `WEFT_JBC_VALUES`/`WEFT_JBC_REPS` — the bench reads its knobs from the environment, and the inputs did nothing before. Run it on demand via:

    ```
    gh workflow run float-roundtrip-ab.yml --repo basic-automation/weftdb --ref main \
      --field values=100000 --field reps=20
    ```

    Triggered only by `workflow_dispatch`. The parse-only figures are the cost statement, and were the only numbers on record until the arms below: 40.0–41.7 ns/value without the feature, 57.0–59.2 with it (≈ +17 ns, ≈ 1.42×, on a Ryzen 9 7950X3D).
  - Measured 2026-11-16 on that workflow, two dispatches at `values=20000 reps=20` (the generated body is 1,414,482 bytes, inside axum's 2 MB default request-body limit, so this bench cannot be widened to the 20.7 MB shape the parse figure used): run **43995374130** `on` p50 15.689 ms / p95 15.983 ms / 784.5 ns/value and `off` p50 16.273 ms / p95 16.508 ms / 813.6 ns/value; run **44003339510** `on` p50 13.660 ms / p95 14.213 ms / 683.0 ns/value and `off` p50 19.728 ms / p95 20.191 ms / 986.4 ns/value. Bench profile is `opt-level=3` with `lto=false`, `codegen-units=16`, not the shipped `[profile.bench]` (`lto = true`, `codegen-units = 1`), which a hosted runner does not finish compiling; both arms share it, so the arms are comparable. The smoke step's 8-value numbers (`8845.6` / `996.4` ns/value) are a path check, not a measurement.    **The whole-request arms do not resolve the cost.** The parse figure predicts an effect of about 20,000 x 17 ns = **0.34 ms** on a request of this size, while the arm-to-arm swing measured was 0.58 ms and 6.07 ms, and in both samples the `off` arm came out the *faster* one — the wrong sign for a feature that only adds work to the parse. So the spread is runner and allocator noise, and the honest answer to the roadmap line is that a whole JSON request does not show a `float_roundtrip` cost at the resolution CI can reach. Reaching better than that needs a self-hosted runner with a warm target directory, which the owner has not provided.
  - Run `cargo test -p weft-server` standalone in CI next to the workspace run (`features` job in `ci.yml`, `--lib`): the release binary is built per package, and feature unification across the workspace hid this bug from every workspace-wide test.
  - Also test `weftdb`'s lib standalone there: 570 passed in 132 s locally with `SKIP_SLOW_TESTS=1`, including the per-package compile.
- **Flaky under GPU contention:** `weft-bench` `gpu_memory::tests::the_pool_stays_bounded_and_stable_on_a_real_gpu` failed in the 2026-11-05 full suite (`stable: false`, one buffer created after round one while 14 GB of the GPU was in use by other work) and passed alone.
- **START HERE (filed 2026-10-11 for the next run).**
- **Research (2026-10-11) — the exact TWA is now a measurable differentiator.**
- **START HERE (filed 2026-10-09 for the next run).**
- **(2026-10-08 list.)** The 2026-10-08 run (PR `routine/dsp-2026-10-08`)
- **(2026-09-23 list; item 3 shipped 2026-10-08.)** The 2026-09-23 run closed both cheap backup
- **POSITIONING ALERT — WeftDB's precision wedge now has a direct competitor (research 2026-09-23).**
  - **NEXT — the remaining 6.3×:**
    - **DONE (2026-10-09, sub-items below) — the `avg` division was the biggest piece of `reduce_scaled`:**

      One idea, 19 digits per `u128` division, was tried and gave no win. What worked was a single `BigUint` multiply and one division per non-terminating average.

  - **NEXT — the sidecar build.**
  - *(original item)* **a `ScaledI64`-native reduction fast path in `weft-reduce`:**
- **DONE (2026-11-02) — External-engine adapter fairness — three concrete rules from the 2026 landscape** (#393): (a) QuestDB over QWP both ways; (b) each ASOF strategy run and published (`clickhouse-double-merge`, `questdb-double-linear`); (c) a precedent, no work
- **Adopt the TSM-Bench QUERY SPEC (not the repo) as Weft-Bench's external comparison workload.**
- **BENCHMARK HYGIENE — the synthetic ingest corpus flatters WeftDB, by a lot (measured 2026-07-23).**
  - Audit the other `weft-bench` workloads' generators for the same class of flattery (the shape
    - **NEXT — a representative storage generator, or real-corpus defaults.**
    - **FOUND (2026-10-09) — `sketch_p99`'s published ~2.6× speed-up over exact `p99` was the
      - **Research (2026-10-09) — a cheaper sketch index, if the sketch format is ever revised.**
      - **Positioning (2026-10-09) — exact percentiles are a differentiator against QuestDB.**
      - **NEXT — select on the mantissas when a partial is not merged.**
- **DONE (2026-11-20) — Audit the other `weft-bench` workloads' generators for the same class of flattery (#396, PR #834).**
  Generated corpus shapes for every generator were read from source and assessed against
  the same flattery classes identified in #395 (2-decimal scale, regular timestamps,
  FOR-friendly repetition, clean analytic signals).  The full write-up is at
  `docs/research/bench-generator-audit.md`.  10 generators were audited (plus the
  real-corpora entry points and size presets); 5 claims in `bench/claims.json` are
  flagged **suspect**, 1 claim (`sketch-claim-corrected`) was already corrected by #398,
  and the remaining claims are `unaffected` — 5 outright, plus the BTC arm of
  `interp-latency`, which splits — because they run on real data or a different code path.  **No claim was withdrawn in this PR** — the suspect verdicts await a
  coordinator decision, and #397 (a representative generator or real-corpus defaults)
  stays open.
- **The seven slow `weft-orchestration` tests are bounded, and the three failures #402 recorded have no subject left (closed 2026-11-11).** Two entries sat here, and `4de2c691` ("the roadmap is a phased checklist; the prose moves to basicautomation.io") cut both mid-sentence, which is why they read as open work: the second, "Re-measured 2026-10-09 — still unbounded", was the tail of the bound-the-seven-tests item, and the first was the note that three failures were hiding behind the `SKIP_SLOW_TESTS` guard.
  - The bound exists. Seven `timed out after 1200s (#404)` guards now sit in the harnesses — `test_api` (`weft-orchestration/src/lib.rs:1772`), `test_pipeline_api` (`:1983`), `test_api_precise` (`:2166`), `test_pipeline_api_precise` (`:2348`), `test_batch_processing` (`:2467`), `test_specific_process_batch` (`:2537`), `test_pipeline_api_with_fake_db` (`:2732`) — at 3x the 357 s in which all seven finished for real on a runner, with push and pull-request runs skipping them so the nightly schedule is what exercises them (`CHANGELOG.md:266`, #404 and #646).
  - #402 is closed as completed (2026-11-10) and recorded here as **closed on erasure, not on a fix**: `test_api_precise`, `test_pipeline_api_with_fake_db` and `test_pipeline_api_precise` exist on `main` only as runner names in `weft-orchestration/src/lib.rs`, the `Invalid time range: start time must be before end time` failure has no fix behind it, and the close-out's own words were "none can be identified — every CI run where the slow weft-orchestration tests actually execute shows all of them passing". The real defect that was fixed is the dictionary one: schema creation now runs in an exclusive transaction before the concurrent one opens, in `weftdb/src/types/database/inputs.rs:1426-1440`, which also restores the three dictionary tables the inline DDL omitted. If those test names turn up again, reopen #402 against the names rather than this entry — that is where the evidence lives.
- **`weft-orchestration`: per-pipeline signal state, not the global `SIGNALS_QUEUE` (filed 2026-11-06, from the fan-controller build).** `SIGNALS_QUEUE` is a process-wide static (`lib.rs:662`); `Pipeline::generate_signals` and `filter_signals_at` clone the whole queue into each pipeline (`pipeline.rs:964`, `:982`). With several sensor pipelines in one process, one sensor's signals reach another's probability queries. Fix: key the queue by aspect (or move it onto `Pipeline`), and add a two-pipeline isolation test. Check whether `create_signals` clears the queue; if not, even running the pipelines one at a time leaks.
  - Done by `929c34a3` [ORCH-SIG] Isolate prediction signals per aspect (#402) (#673): the queue is `static SIGNALS_QUEUE: LazyLock<Mutex<HashMap<AspectId, Signals>>>` (`weft-orchestration/src/lib.rs:667`) and `signals_for` copies out one aspect's queue (`:671`), so `Pipeline::generate_signals` and `filter_signals_at` (`weft-orchestration/src/pipeline.rs:964`, `:980`) pick up their own signals only. The requested two-pipeline isolation test is `signals_are_isolated_per_aspect`, which queues a signal for aspect A, then creates, filters and loads a pipeline over B and asserts A still holds exactly one. The item's last sub-question, whether `create_signals` clears the queue: it does not — it takes `queue.entry(*aspect_id).or_default()` (`lib.rs:1369`) and accumulates, which is the design rather than the leak the item feared, since it reads only unprocessed events and `filter_expired_signals` drains what has expired.
- **`weft-orchestration`: bounded-window threshold detectors (filed 2026-11-06, from the fan-controller build).** `detect_threshold_crossing_up` (`detectors.rs:504`), and probably its sibling detectors, analyse the aspect from its earliest to its latest measurement on every call, then collect and sort every point. For a 1 Hz daemon that runs for weeks, each tick costs O(history). Fix: an optional `since` start time (or an incremental cursor from the last evaluated point), so a live caller only scans new data. Add a test that the windowed and full-history runs find the same crossings in the window. *Closed 2026-11-11 (#692, `cfefb688`): eight `*_in_window` variants take `since: Option<DateTime<Utc>>` and bound the analysed range at its start while its end stays the aspect's latest measurement; `scan_range` turns a window that opens at or after the latest measurement into an empty scan instead of a reversed range; every family now has a test whose `since` falls inside its fixture, so an implementation that ignored `since` fails. What a window costs: it can only miss a feature, never invent one — `detect_drawdown` re-bases its running high on the window's first point, so a windowed decline is never larger than the full-history one.*
- **`weftdb` dictionaries: shrink the occurrence footprint without dropping any occurrence (filed 2026-11-06, after the Fan Control 112 GB incident).** Every occurrence must be kept, because the statistics need them all. No `max_patterns` or `max_occurrences` caps: if the dictionary needs a cap, it is under-constrained. The duplicate-on-every-run bug is **fixed 2026-11-07 (#612, 0.3.2)**: `Pipeline::import_windows` rewrites every id, so the old `if exists { continue }` test could not fire and each run re-inserted every occurrence. What remains: *Done in 0.3.3 (#699): `OccurrenceLog` keeps every occurrence losslessly at about 3 bytes each, in memory and on disk.*
  - **Lossless compact encoding.** Each `pattern_occurrences` row now repeats `pattern_id`, `aspect_id`, `resolution`, `size`, a JSON `database_info` and two timestamps. Store the per-dictionary constants once. Derive `end` from `start + size·resolution` and store it only when it differs. Store `start` as delta-of-delta varints, reusing the `weft-physical-type` timestamp codecs. Keep each pattern's occurrences in an append-only log on disk.
  - **Lean patterns in memory.** A pattern in RAM holds its shape, `count` and `first_seen`/`last_seen`. Occurrences are never loaded for matching; statistics and correlation read them as a stream. First check what `correlation.rs` needs from them.
  - **Restore on reload.** `load_dictionary` restores the stored patterns verbatim, with no re-matching through `import_pattern` and no occurrence load.
  - **Keep merging into every matching pattern**, as today. Any statistic should be computed per window, not by summing pattern counts.
  - **Learn the constraints when they are unset.** One shared radius λ, adjusted toward a target rate of new patterns. Each variability kind gets T_k = λ·(online median of that kind's nearest-neighbour distance). Steps and interpolation are learned together once, then frozen. `Steps.Enforced` defaults to true. Sum kinds are derived as N·avg. Percentile kinds are used only when |mean| ≫ noise. The packing number bounds the pattern count; warn if it keeps climbing.
  - **Tests:** replay the incident (1 window per minute) and check that `preamble.db` grows only linearly, at the compressed rate. Check that save then reload restores the dictionary exactly, and that there are zero duplicate occurrence rows.
- **`weftdb` memory per database file: RSS keeps climbing across many small stores (filed 2026-11-08, from the Fan Control daemon on 0.3.2).** The daemon's store holds 529 database files, and its RSS keeps rising after the dictionary-doubling fix. Measure before changing anything; two hypotheses: *Done in 0.3.3 (#699): idle store files close after `WEFT_DB_IDLE_CLOSE_SECS` and the heap is trimmed; 2.44 GB to 21 MB on a copy of that store.*
  - **Page cache.** `configure_database_for_mvcc` (`database/connection.rs:119`) sets `PRAGMA cache_size=-256000` (~256 MB). If Turso applies it per *database*, 529 files allow about 135 GB, filled as pages are touched and never evicted below that. But it runs on a throwaway connection, and the comment above it says per-connection pragmas set there change nothing, so it may not apply at all. Confirm how `turso_core` scopes `cache_size`.
  - **MVCC version store.** Every file opens in `journal_mode=experimental_mvcc`, which keeps row versions in memory until a checkpoint. Many files that are written often and checkpointed rarely would show the same slow climb.
  - **Fix direction:** a process-wide memory budget split across open databases, not a fixed size per file. A small default `cache_size` (a few MB) on every working connection, raised only for bulk loads. Close or idle out databases that are not in use. Checkpoint the MVCC store on a schedule. Also question why one aspect-heavy app needs 529 files (one per aspect × dictionary): consolidating them is the bigger lever.
  - **Test:** open N=500 stores, write to each at 1/min for a simulated day, and assert that RSS stays under a configured budget and does not grow with N × cache_size.
- **DONE (2026-11-10) — `weft-orchestration` is published, current at 0.3.2, verified through the crates.io sparse index (filed 2026-11-06).** `publish = false` would be a regression: it would withdraw a crate people already depend on. The remaining risk is the path/crates.io mixing hazard — two copies of `weftdb` in one binary, so the types do not unify — now documented in `weft-orchestration/README.md`: depend on **both** `weftdb` and `weft-orchestration` from crates.io, or vendor **both** by path, never one of each. `weft-physical-type`, `weft-reduce`, `weft-line-protocol` and `weft-arrow` sit at 0.3.0 while `weftdb`, `weft-arrow-store` and `weft-orchestration` are at 0.3.2; that lag is release hygiene, not a reason to unpublish.
- **Historical note — the LNK1102 research that produced the `[profile.test] debug = 1` fix:**
  - *Audit 2026-10-27: checked — this is a historical note and its fix shipped: `[profile.test] debug = 1` is at Cargo.toml:128.*
- **DONE (2026-11-13) — Guard the `#[serial]`-above-`#[test]` ordering (#726).** The keyed form is what
  the guard was missing: its regex was `#\[\s*serial\s*\]`, which matches only the plain attribute, so the
  28 `#[serial(key)]` sites — the ones that guard the process-global fault injectors — were outside it
  entirely. What the guard now enforces is "a `#[serial]` shares an attribute block with `#[test]` or
  `#[tokio::test]`", not "immediately above". `#[tokio::test]` re-emits the item's outer attributes before
  the `#[test]` it generates (`tokio-macros/src/entry.rs`), so `#[tokio::test]` + `#[serial(k)]` expands to
  `#[serial(k)]` + `#[test]` over a sync body and serialises through `local_serial_core`; `#[serial]` above
  `#[tokio::test]` serialises through `local_async_serial_core`. The old docstring's "silently loses the
  serialisation" was therefore wrong. The real silent failure is a `#[serial]` in a block with no test
  attribute at all — it compiles, passes, and runs concurrently — and that is what fails CI now. Scan of
  all 355 tracked `.rs` files: no violations, so no site needed moving.
- **DONE (2026-11-14) — ROB-22 Suspect-frame policy and background-decode crash-loop breaker (#720).**
  The slice adds three things. First, `SuspectFramePolicy` (`weftdb/src/types/quarantine.rs`): an enum
  with three variants (`Skip`, `Quarantine`, `Fail`), parsed from `WEFT_SUSPECT_FRAME_POLICY`, and a
  `decode_failure_action` function that applies the policy to a C5-classified failure — corrupt frames
  are always quarantined, suspect frames follow the policy. Second, a decode circuit breaker alongside
  the existing maintenance breaker: `SegmentStoreOptions::decode_breaker`, the
  `decode_breaker_state` map on `SegmentStore`, `decode_breaker_permits`,
  `decode_breaker_record`, and `decode_breaker_count_failure` methods on the maintain impl, plus
  `WEFT_DECODE_BREAKER_FAILURES` / `WEFT_DECODE_BREAKER_COOLDOWN_SECS` in the registry. Third, the
  env-var registration: `WEFT_DECODE_BREAKER_COOLDOWN_SECS`, `WEFT_DECODE_BREAKER_FAILURES`, and
  `WEFT_SUSPECT_FRAME_POLICY` are all registered in `weft-server/src/settings.rs` (in sorted position
  with PAC-5b strict validation), documented in `docs/config-schema.md` (forty-nine Tuning variables,
  112 total) and ticked in `ROADMAP.md:92`. The breaker methods replicate the maintenance breaker's
  tripping logic on a separate state map with its own threshold and cooldown, so decode failures on a
  damaged frame do not contaminate the sweep-level tripped-aspect list.
  What is **deliberately left out**: the integration of the decode breaker into the daemon sweep entry
  points and the interactive read path — those call sites are in `weft-server/src/*daemon*.rs` and the
  maintenance code path, and wiring them was the part the first worker stopped on. The policy type and
  breaker API are ready for those callers; which daemon loop checks `decode_breaker_permits` before
  decoding and calls `decode_breaker_record` after is the next slice's decision, and was too speculative
  to commit to without the daemon author's input.
- **NEXT — UDDSketch (uniform guarantee under collapse):**
  - The sketch itself, as a library type (2026-11-11): `weft_reduce::UddSketch` (`weft-reduce/src/uniform.rs`). Tests pin the uniform bound after collapse against an exact sort, the closed form of the bound, and exact merging under collapse.
  - A bench for the cost against `DdSketch` (2026-11-11): `cargo bench -p weft-reduce --bench sketch_collapse`, two seeded synthetic inputs of 100,000 samples at 1%. First figures, criterion medians on a Ryzen 9 7950X3D **while the host ran at a load average near 100**, so they are indicative and not a claim. `add`, random walk, no collapse at 2048 buckets: `DdSketch` 0.65 ms, `UddSketch` 0.44 ms (three runs each, within 1%); `UddSketch` took 1.83 ms in the one run before it had `DdSketch`'s last-bucket cache. `add`, eighteen decades at a 256-bucket budget, both collapsing: 6.7 to 9.6 ms against 6.2 to 7.0 ms over five runs, no clear difference. `merge` of 64 collapsed sketches: 0.67 to 0.75 ms against 78 to 101 µs. What the speed buys on that input: `UddSketch` collapsed four times and reports a 15.9% bound on every quantile, where `DdSketch` keeps 1% on the quantiles above its folded buckets and no bound below.
  - The accuracy each collapse costs on real data (2026-11-11; deterministic, so the host load does not touch it). Every BTC/USD one-minute close in `database/datasets/btc_1min.csv` (7,146,457 samples, 3.8 to 123,236, about 520 buckets wide at 1%), relative error against an exact sort at p1 / p50 / p99, printed by the same bench. 256 buckets: `DdSketch` 151× / 0.75% / 0.81%, `UddSketch` 3.7% / 0.17% / 2.2% (2 collapses, bound 4.0%). 64 buckets: 7,092× / 412% / 0.81% against 15.5% / 4.9% / 8.9% (4 collapses, bound 15.9%). 16 buckets: 18,525× / 1,236% / 0.81% against 13.3% / 50.8% / 47.1% (6 collapses, bound 56.5%). So `DdSketch` holds p99 at every budget and loses everything below its folded buckets; `UddSketch` is never unbounded and is worse at p99. For a p99-only reduction the lowest-bucket collapse is the better trade.
  - Repeat the timing figures on a quiet host with pinned cores before citing any of them.
  - **DONE (2026-11-14) — TimescaleDB's default percentile aggregate is a UDDSketch, so it is the like-for-like comparison.** `percentile_agg()` "calls `uddsketch` with 200 buckets and a maximum error rate of 0.001" (<https://www.tigerdata.com/docs/reference/toolkit/percentile-approximation/uddsketch/percentile_agg>); the `uddsketch()` page says "the true error may exceed this if too few buckets are provided for the data distribution" (<https://www.tigerdata.com/docs/reference/toolkit/percentile-approximation/uddsketch/uddsketch>); and `error()` returns the bound actually reached (<https://www.tigerdata.com/docs/reference/toolkit/percentile-approximation/uddsketch/error>). That page's own sample, `uddsketch(100, 0.001)` over 0..100, returns 0.007999832004199894, which is `tanh(8·atanh 0.001)` to the last digit: the closed form `UddSketch::relative_accuracy` uses, at three collapses. The full research document is at `docs/research/timescale-percentile.md` (#732). Two things to do with it: (a) when the TimescaleDB adapter lands (line "Add ClickHouse, InfluxDB 3, QuestDB, TimescaleDB adapters"), run its percentile at those defaults and record `error()` beside the timing, since comparing its speed against `sketch_p99` at 1% and 2048 buckets without the achieved error would flatter one side; (b) run `UddSketch::new(0.001, 200)` over the BTC closes in `sketch_collapse` and print the bound. By the closed form that corpus (about 5,190 buckets wide at 0.1%) needs five collapses and lands near 3.2%, against the 1% `sketch_p99` holds without collapsing; that figure is computed, not measured.
  - Decide whether the `sketch_p*` reductions take it. At `SKETCH_MAX_BINS` = 2048 a realistic column never collapses, so the two differ only on pathological ranges; switching changes the `.weftpart` sidecar bytes (postcard is not self-describing), so it needs a sidecar version step. Owner-gated, like the other default-codec changes.
- **DONE (2026-07-18) — materialize per-segment partials at seal time (the highest-value

  Per-segment `.weftpart` sidecars are written at seal time at a declared base resolution. `downsample_range` merges the sidecars instead of decoding whenever every requested reduction allows it, a fine base can be re-bucketed to a coarser resolution, and the sidecars stay consistent across segment rewrites.

- **NEXT — (3c) materialized tiers, remaining:**
- **Positioning — WeftDB's `.weftpart` sidecar IS the "incremental materialized view", and mergeable
- **NEXT — materialize the sidecar at a coarser DEFAULT + adopt sketches:**
- **NEXT — default target for the compaction daemon:**
- Add the **DuckDB** adapter
  - **Research (2026-10-09) — DuckDB has a native linear-interpolation arm:**
- Add ClickHouse, InfluxDB 3, QuestDB, TimescaleDB adapters
- Add end-to-end timing spans *(harness-level spans shipped; per-pipeline-stage spans = Phase 3 tracing item)*
- **NEXT — upstream FastLanes does NOT make the transposed layout its default either, and
- **DONE (2026-10-08) — Cross-check WeftDB's hand-rolled bit-plane decoder against the `fastlanes` crate (0.7.2,
  - **Residue — still 2.4–7× behind `fastlanes` (0.33 ms flat across widths).**
- **Track the FastLanes SPEC, not the CWI reference implementation.**
- **Next slice — evaluate Vortex as interchange + bench baseline (Phase 1/4):**
  - **GO on both counts, and the governance question is settled (research 2026-09-23):**
  - **Do NOT take a build dependency on `vortex-gpu`.**
- **Evaluate the FastLanes *File Format* (not just the layout) as a bench baseline + a
  - **It publishes a random-access target WeftDB can mirror exactly (research 2026-09-23):**
  - **Two honesty corrections from the same paper.**
- **Point-lookup positioning — WeftDB's block-random-access read is a genuine columnar mitigation
- **GPU-decode the `.weftseg` transposed layout rather than adopting a new format (Phase 5/6):**
- **Correlation-aware column compression as a post-B-tags codec direction (Phase 6):**
- **Cascade adopt-or-drop into the DEFAULT selector — owner-gated (headline change, as FOR/ALP):**
- **NEXT — checkpoint index adopt-or-drop into the DEFAULT policy — owner-gated (bytes/point
- **NEXT — a random-access-capable Gorilla/RLE would widen the checkpoint index's reach:**
- Publish a methodology document **before** any performance claim

## Panic audit (2026-11-14, P242, #242)

**Phase 14 item ticked.** Static-grep audit of every panic-capable construct in
`weft-server/src/` and `weftdb/src/` reachable from HTTP request handlers. Methodology:
searched for `unwrap`, `expect`, `panic!`, `unreachable!`, `todo!`, `assert!`, fallible
array indexing, `Mutex`/`RwLock` poisoning, and `SystemTime` arithmetic, then verified
each site's reachability and triggerability.

**Catch boundary:** `panic_catch::catch_panic` middleware wraps all request handlers
(lib.rs). Non-ingest panics → `ApiError::internal` (500). Ingest panics →
`map_ingest_panic` per-phase retry or 400/500. Control-plane writes aborted by ROB-2
hook before catch layer; daemon panics restart via `daemons.rs` supervisor.

**Result: 13 OK-safe, 1 unreachable, ~18 test-only, 0 NEEDS-FIX.**

**Cleared on review:** `weftdb/src/types/database/outputs.rs` in `analyze_point` was
first filed as NEEDS-FIX because `bail!("No measurements found for aspect")` was taken to
expand to `panic!()`. It is `anyhow::bail`, which expands to `return Err(anyhow!(…))`
(`anyhow-1.0.104/src/macros.rs:58`), and the function already returns
`anyhow::Result<Point>` — so an empty result set propagates an `Err` and the
`.last().unwrap()` at line 402 is guarded by that same early return. Issue #748, filed from
the mistaken premise, is closed as not-a-bug.

**Other production panics are safe by construction:**
- `hash[..8].try_into().unwrap()` — SHA-256 output guaranteed 32 bytes.
- `NonZeroUsize::new(10_000_000).expect(...)` — literal > 0.
- `permit.expect("the semaphore is never closed")` — accept loop semaphore never closed.
- All `unwrap_or_else(PoisonError::into_inner)` — converts poison to `Err`, no panic.
- All `SystemTime::now().duration_since(...)` — use `map_or(0)` or `?`, never bare unwrap.
- Array index `[0]` in `simplify_transformation` — guarded by early returns.
- `last().unwrap()` in compaction/simplification — guarded by `len() <= 2` early returns.

**Not audited** (out of scope): `compat/`, `weft-client/`, `weft-tui/`, `weft-bench/`,
`weft-xbench/`, `benches/` directories, `clients/`, test-only files.
## Host probe (run 2026-11-14 on DeepThought, before the worker starts)

```
$ unshare -Um true
exit=1
unshare: unshare failed: Operation not permitted
$ unshare -rm true
exit=1
unshare: unshare failed: Operation not permitted
$ df -h /tmp
/dev/nvme0n1p2  3.7T  2.8T  729G  80% /tmp
```

Conclusion for this host: **user and mount namespaces are unavailable** (`unshare -Um true` fails with
EPERM), so Strategy A (`mount -t tmpfs -o size=1m` inside a namespace) cannot run here and must skip
honestly with that reason printed. The figure the guard compares against is injectable through the
public `weftdb::DiskSpace` / `DiskGuard::admit` pair (`weftdb/src/types/disk_guard.rs:29`, `:138`), so
Strategy B pins the refusal without privileges. There is no `WEFT_DISK_USAGE_COMMAND` or any other
command/env seam for filesystem usage anywhere in this repository — the only operator knob is
`WEFT_DISK_RESERVE_BYTES`, and the library-side injectable figure is the `Option<DiskSpace>` argument
of `admit`.

## ROB-20 G5 — per-operation wall-clock budget for maintenance sweeps

Code: `weftdb/src/types/segment_store/maint.rs` — the `maintenance_deadline()` helper computes an
`Option<Instant>` once per sweep and each held path (`reconcile_overlaps_held`,
`squash_series_held`, `squash_aspect_to_target_rows_held`) checks it between components/groups;
the four store-wide sweep loops (`reconcile_all_over_threshold_with_cancel`,
`reconcile_all_hot_cold_with_cancel`, `squash_all_over_threshold_with_cancel`,
`squash_all_to_target_rows_with_cancel`, `squash_all_to_target_rows_if_fragmented_with_cancel`)
check it between aspects.

Test: `maintenance_budget_tests::a_maintenance_sweep_stops_at_its_time_budget_before_any_swap`
— sets `with_max_maintenance_op_seconds(0)` so the deadline is `Some(Instant::now())` and the
loop breaks before any swap. The converse (unset ⇒ proceeds) is covered by every existing sweep
test that completes normally without setting the env var.

Env var: `WEFT_MAINTENANCE_MAX_OP_SECONDS` (`Tier::Experimental`), parsed at
`SegmentStoreOptions::from_env()` in `segment_store.rs:538-539`. Unset = unbounded.

## Routine 2026-11-19 — Phase 12 wrapper reconcile, Turso re-check, handoff

**Wrapper reconcile.** The six empty wrapper lines at the top of Phase 12 in `ROADMAP.md` were removed: the four `START HERE` lines (2026-11-11 filed, 2026-11-11 rewritten, 2026-10-11, 2026-10-09) and the two dated routine lists (2026-10-08, 2026-09-23). They had no sub-items left. Each item their prose (under "Immediate next actions" above) still names was checked against `ROADMAP.md` before the delete:

- UDDSketch timing rerun on a quiet host: the open sub-item under **NEXT — UDDSketch** in Phase 25.
- Real-filesystem ENOSPC test for #656: ticked in Phase 12.
- Select on the mantissas when a partial is not merged (#401): the open item in Phase 25.
- Dictionary occurrence footprint: shipped in 0.3.3 (#699), as recorded above.
- The GPU-contention flake and the `float_roundtrip` whole-request figure: both ticked in Phase 12.
- Memory per database file (the RSS question filed 2026-11-08): no `ROADMAP.md` line. It stays as prose above, because the owner's `p12/fanctl-correlation-growth` lane is working on it now (per-row memory probe, connection reuse). No item was added, to avoid claiming it.

The matching empty bold headers in this file ("START HERE (filed 2026-10-11 …)", "(2026-10-08 list.)" and so on, under "Immediate next actions") are left as they are. They are a record, and this run did not change those lines.

**Research: Turso MVCC checkpoint fix, re-checked 2026-11-19.** All primary sources:

- [tursodatabase/turso#7480](https://github.com/tursodatabase/turso/issues/7480) is still **open**.
- The fix, [turso#9770](https://github.com/tursodatabase/turso/pull/9770), is still **unmerged**. A stale-bot warning on 2026-11-16 said it would close after 7 days without activity. The author rebased it onto current `main` on 2026-11-17, with the regression test moved to `tests/integration/mvcc/database.rs`. Turso's automated reviewer marked it "looks good to merge" on 2026-11-18, but no maintainer has approved it.
- [Releases](https://github.com/tursodatabase/turso/releases): stable is still **0.8.3** (2026-10-12). The newest prerelease is 0.8.4-pre.3 (2026-11-12), and its notes have no MVCC checkpoint or logical-log line. The workspace pins `turso = "0.8"` (resolved 0.8.1).
- Conclusion: nothing has changed for this repo. The non-Linux SIGKILL exclusion stands. Watch #9770 for a merge before the stale window closes again, and then for the first release that carries it.

**Handoff for the next run.** Phase 12's open items are all held: ROB-14 (the `p787` and `p810` lanes, active 2026-11-17; cut 2 merged in #805), TTL-PURGE and TTL-UNIT (#810, the `p799` worktree), and SEAL-* (#811–#819). The rest are owner-gated. In Phase 13, the TAG-12 fan-out arm is held by #809. TAG-12's TSBS half (`single-groupby-1-1-1` and `lastpoint` over the whole TSBS file tagged by `hostname`) still needs a quiet host. On 2026-11-19 the host had a load average of 36 and 9 GB of 124 GB available, with a 59 GB `strata` process resident.

## 2026-11-20 — OpenAPI breaking-change check wired (FRE-18a, #838)

The rc.1 wire-compatibility baseline was cut at `compat/openapi/baseline/weftdb.json`
(a byte-identical copy of the committed spec at the freeze cut).
`scripts/ci/check-openapi-breaking.py` compares the live committed spec against the
baseline and classifies every difference against the six rule families in
`docs/stability.md:27-40`. The check and its unit tests run in the `claims` CI job.
## Routine run (2026-11-17): 7.3/7.5 reconcile, TAG-12 multi-series crash cell, Turso and TSBS research

**Reconcile.** 7.3 and 7.5 were ticked from their parts, each pinned by a test re-run on `435553e` (47 passed); see DECISIONS 2026-11-17. The six empty routine wrappers at the top of Phase 12 were deleted; their prose stays above under *Immediate next actions*.

**TAG-12, the multi-series crash matrix.** Three new cells in `Failure::ALL` (now six), so `--crash-matrix`, `scripts/crash-matrix.sh` and `crash-matrix.yml` pick them up with no wiring. All three use three series `host=s0..s2` of one aspect, each storing corpus value + series number, so a row from the wrong series reads wrong. The checks are per series (`resolve_series` + `read_time_range_of`).
- `kill-ingest-tagged`: SIGKILL after a seeded number of acknowledged tagged ingests. Each series holds exactly its acknowledged rows (only the in-flight series may hold one batch more), then everything resumes to whole.
- `kill-compaction-tagged`: SIGKILL a seeded number of ms into compacting interleaved series. `fsck` is clean, every series is exact, and a rerun stays exact.
- `corrupt-segment-tagged`: one flipped byte. Served rows are right, losses are quarantined, and at most one series loses rows. Every observed run quarantined the flipped segment and lost 5 rows from one series only.
- Each cell fails a wrong-series mutant (`crash_matrix_tests` for the ingest cell; the real CLI for the other two).
- **Not measured:** the matrix's CI wall-clock. The tagged cells do about 3× the ingest work of their untagged twins, and `scripts/crash-matrix.sh` defaults to `--rows 100000 --reps 2 --seeds 3`. The first `crash-matrix.yml` run after merge is the measurement; if it runs long, lower `--rows` for the tagged cells instead of dropping them.
- **Runtime evidence:** this run's scratch dir `/mnt/deepmem/Development/weftdb-routine/runs/20261117-025714/` (logs `test-weft-bench-full-3.log`, `good-compaction.log`, `mutant-compaction.log`, `good-corrupt.log`, `mutant-corrupt.log`). The reopen times in those logs come from spinning disk on a box at load 24–33, so they are not a performance figure.

**Research: the non-Linux SIGKILL exclusion still stands.** [tursodatabase/turso#7480](https://github.com/tursodatabase/turso/issues/7480) (an MVCC checkpoint truncates the logical log before the WAL, and the store then refuses to open with `Corrupt("WAL has committed frames but logical log header is missing")`) is **open**, last updated 2026-11-13. A commenter confirmed that a plain SIGKILL during an ordinary `wal_checkpoint(TRUNCATE)` reaches the state. The candidate fix, [turso#9770](https://github.com/tursodatabase/turso/pull/9770) ("complete an interrupted TRUNCATE checkpoint when the log is empty"), is **open and unmerged** (updated 2026-11-16). A maintainer's interim advice is to open such databases in experimental passive-checkpoint mode ([turso#9749](https://github.com/tursodatabase/turso/issues/9749), also open). `crash_matrix_tests.rs` deliberately does not take that route. The [release list](https://github.com/tursodatabase/turso/releases) has v0.8.3 as latest stable and v0.8.4-pre.3 (2026-11-12) and v0.9.0-pre.2 as pre-releases; this workspace pins `turso` 0.8.1. Nothing here clears the exclusion. Re-check when #9770 merges and ships in a stable release.

**Research: what the TSBS half of TAG-12 has to run.** The [TSBS README](https://github.com/timescale/tsbs) (repo active, pushed 2026-11-16) defines the devops/cpu-only queries over the `hostname` tag: `single-groupby-{1,5}-{1,8}-{1,12}` (MAX of 1 or 5 metrics for 1 or 8 hosts, every 5 min, over 1 or 12 h), `double-groupby-{1,5,all}` (mean per host per hour over 24 h), `high-cpu-{1,all}`, `lastpoint` and `groupby-orderby-limit`. `weft-xbench --tsbs` keeps only `host_0`'s `usage_user` today, so the suite needs the whole file ingested tagged by `hostname`. That is now possible (TAG-11), and the `8`-host variants are the fan-out case.

**Next (tomorrow).** Every TAG-12 sub-item left needs either a quiet host or a spec:
1. TSBS `single-groupby-1-1-1` and `lastpoint` as the first two tag-grouped queries in `weft-xbench`, after ingesting the whole TSBS file tagged by `hostname`.
2. The fan-out benchmark. Both (1) and (2) are benchmarks, so they need a quiet host; this run's load average was 24–33 on 16 cores with the model server resident.
3. "Tagged fixtures and freeze docs" needs the owner to say what it means. The tagged frame golden already exists (`weft-physical-type/tests/golden/frame_v7_envelope_series_key.bin`); a tagged whole-store compat fixture beside `weftdb/tests/fixtures/pre_v2_store` is the likely reading.

**TSM-Bench query spec adoption (P394, 2026-11-20).** ROADMAP.md:246 ticked.
`docs/research/tsm-bench-query-spec.md` published: Q1–Q7 canonical SQL extracted from the
Khelifati et al. PVLDB 2023 paper; Garcia/Seltzer 2015 source unverifiable behind Cloudflare.
Gap analysis: Q5 (`--gap-fill`) is the only existing arm that explicitly identifies itself as a
TSM-Bench query; Q1 (`--range-fetch`) is the next closest; Q2/Q3/Q6/Q7 need new arms.
Staged adoption plan with five stages (Q5 formalization → Q1 adaptation → Q4 stored-data path
→ Q2/Q3/Q6/Q7 new arms → cross-engine gate). No cargo / no Rust files touched.

## Routine 2026-11-21 — xbench lock, TSBS reference queries, Turso #9770

**Fixed: the nightly cross-engine smoke was red on `main`.** Every scheduled `xbench-smoke.yml` run from 2026-11-17 to 2026-11-20 failed in about 90 s at `cargo build --locked --manifest-path weft-xbench/Cargo.toml`: `weft-xbench/Cargo.lock` never recorded `tempfile`, which `weft-bench` depends on. `scripts/xbench/check-lock.py` compares only the packages the two locks share, so the per-push `xbench-lock` job stayed green. The lock gained the one line, and that job now also runs `cargo metadata --locked` against `weft-xbench` (exit 101 on main's lock, 0 on the fixed one). `cargo clippy --all-targets` for `weft-xbench` was also failing on main, on an `Option::map` with a unit closure in a `render.rs` test, and that is fixed too.

**TAG-12, the TSBS half: answers before timings.** `weft-xbench/src/tsbs.rs` reads every host's ten `cpu` fields (`load_cpu`) and answers the six `single-groupby-*` variants and `lastpoint` in process (`TsbsCpu`, `TsbsQuery`). `weft-xbench/src/weft.rs` `TsbsWeft` stores the same data tagged by `hostname` and answers both query types from the store, and a test pins its answers equal to the reference over 10 hosts × 720 readings. Nothing is timed and no number exists. See DECISIONS 2026-11-21 for why the buckets are one minute.

**Research.**
- TSBS (repo head `c08b6b9`, 2026-11-17). Read: `cmd/tsbs_generate_queries/main.go`, which registers the six `single-groupby` variants; `uses/devops/common.go` (`cpuMetrics`, in order); and the `GroupByTime`/`LastPointPerHost` generators in `databases/{timescaledb,clickhouse,influx,questdb}/devops.go`. All four bucket `single-groupby` by one minute (`time_bucket('60 seconds')`, `toStartOfMinute`, `group by time(1m)`, `SAMPLE BY 1m`). `lastpoint` is the last row per host in every one: QuestDB `latest by hostname`, InfluxDB `group by "hostname" order by time desc limit 1`. The README's "every 5 mins" is [timescale/tsbs#214](https://github.com/timescale/tsbs/issues/214) (open since 2022-11-23, no comments). Sources: https://github.com/timescale/tsbs/blob/master/cmd/tsbs_generate_queries/databases/timescaledb/devops.go, https://github.com/timescale/tsbs/blob/master/cmd/tsbs_generate_queries/main.go, https://github.com/timescale/tsbs/issues/214.
- Turso: [tursodatabase/turso#9770](https://github.com/tursodatabase/turso/pull/9770) ("complete an interrupted TRUNCATE checkpoint when the log is empty") **merged 2026-11-19**, and [#7480](https://github.com/tursodatabase/turso/issues/7480) is **closed**. The fix is in `v0.9.0-pre.6` (2026-11-20; the compare API reports the tag 110 commits ahead of the merge commit), but **not** in the new stable [`v0.8.4`](https://github.com/tursodatabase/turso/releases/tag/v0.8.4) (2026-11-20). That tag diverged from `main` before the merge, and its 36 commits carry two other MVCC logical-log fixes ("keep late commits in the logical log on passive-flag TRUNCATE", "publish upgraded logical-log header only after its write succeeds") but not #9770. This workspace pins `turso` 0.8.1. So the non-Linux SIGKILL exclusion in the crash matrix stands until a stable release with #9770 ships, or until the owner chooses to move to 0.9.x. A 0.8.1 → 0.8.4 bump is the owner's decision; the two logical-log fixes above are the case for it.

**Next (tomorrow).**
1. A timed TSBS-query phase in `weft-xbench` (`--tsbs-queries`?): load the file into `TsbsWeft`, time each query over generated hosts and windows, and gate each answer against `TsbsCpu`. This needs the report schema to carry a per-query result, so it is about one run. Publish timings only from a quiet host.
2. `lastpoint` on WeftDB reads each series whole. A latest-row read (one segment, the last row) is the fast path, and it is worth checking whether `SegmentStore` already has one before adding one.
3. Phase 12's **Row-TTL purge defects** parent has all three sub-items ticked and #785's daemon call site shipped (#786). It can be ticked by whoever next edits Phase 12; this run did not touch Phase 12.

