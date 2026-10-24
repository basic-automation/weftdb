# WeftDB Roadmap

## Honesty pages

- [ ] Ship honesty pages

## Phased roadmap (0–9)

### Phase 0 — Benchmark thesis & buyer wedge · *Immediate*

- [x] Public performance thesis + first benchmark-claim list *(the [Commercial thesis](https://basicautomation.io/projects/weftdb/docs/commercial-thesis) doc)*
- [x] Define what WeftDB is **not** chasing first
- [ ] Identify 3–5 design partners with interpolation-heavy workloads
- [ ] Document willingness-to-pay hypotheses

### Phase 1 — Build `Weft-Bench` · *Highest priority*

- [x] `weft-bench` first-class workspace member (builds + tests green)
- [x] First workload profile
- [x] WeftDB adapter over the vendor-neutral `SystemAdapter` trait (drives `splimes`)
- [x] Portable baselines
- [x] Accuracy scoring vs analytic ground truth (RMSE/MAE/max-error/bias)
- [x] Shape-selectable synthetic truth (`MultiSine`/`Sawtooth`/`Step`/`DampedSine`)
- [x] Latency stats
- [x] Harness-level timing breakdown (`BenchResult.timing`)
- [x] JSON report output
- [x] HTML report output (`--html`, self-contained) + best-effort hardware probe (CPU model/cores/RAM via `sysinfo`)
- [x] Storage estimate per result (`StorageEstimate`: recommended encoding + value/timestamp/total bytes/point)
- [x] CLI runner
- [ ] External-engine adapters
- [ ] Full TSBS-compatible comparison harness (ILP parser shipped; harness pending)
- [x] **Storage/read/aggregation workloads shipped (2026-07-14)**
- [x] **DONE (2026-10-09) — `gap_fill` workload** (`--gap-fill`)
  - [ ] **NEXT — `?fill=` on the downsample endpoints** (`POST /api/v1/downsample`, `GET …/storage/{aspect}/downsample` and their columnar siblings), with a …
  - [ ] **NEXT — arbitrary bucket widths** (`5s`, `15m`)
  - [x] **DONE (2026-10-09) — gap fill on a real corpus** (`--gf-csv`)
  - [x] **MEASURED (2026-10-09) — a spline fill (`Fill::Spline`, `--gf-fill cubic|quadratic`) does NOT beat linear on real prices.**
  - [ ] **NEXT — a real smooth-signal corpus for the spline-fill claim.**
- [ ] Remaining workloads
- [x] **DONE (2026-10-09) — first-rep/warm separation** (`BenchResult.cold_warm`, schema v21, every workload; console + Parquet)
  - [x] **EXPLAINED (2026-10-09) — the compression first rep is first-touch memory, not lazy init.**
  - [ ] **NEXT — OS-cold and post-restart runs:**
- [ ] Fair-protocol depth (Phase 1.1)
- [ ] Fair interpolation comparisons (Phase 1.2)
- [x] **DONE (2026-10-08) — disk capture:**
- [x] **DONE (2026-10-09) — GPU and driver versions in run metadata:**
- [x] **DONE (2026-10-09) — Parquet report surface:**
- [ ] Report surfaces beyond JSON/HTML/Parquet
- [ ] Anti-Goodhart (Phase 1.3)

### Phase 2 — Benchmark-grade server/API · *Very high*

- [x] `axum` server skeleton
- [x] Flagship `POST /api/v1/interpolate` over the `splimes` engine (raw/interpolated/extrapolated provenance on every point)
- [x] InfluxDB-Line-Protocol ingest
- [x] Downsample/aggregation query
- [x] Single-instant point query
- [x] Stored-range read surface
- [x] Single-instant read surface
- [x] Storage maintenance
- [x] HTTP catalog management + ingest
- [x] B-rest pagination
- [x] Parquet interchange
- [x] CSV interchange
- [x] Columnar output for compute endpoints
- [x] Prometheus latency histograms
- [x] OpenTelemetry trace export (paired with Prometheus `/metrics`)
- [x] B-rest residue
- [x] B-rest residue
- [ ] Python SDK → Rust SDK → Arrow Flight / Flight SQL → SQL surface / DataFusion (later)
- [ ] Grafana → Prometheus remote write/read (if monitoring) → R/Arrow workflows

### Phase 3 — Instrument everything · *Very high*

- [x] Prometheus `/metrics` surface
- [x] Harness-level timing breakdown (dataset-gen + adapter + whole-run spans)
- [x] Latency histograms over the compute + seal paths (`/metrics`)
- [x] `/debug/profile/current`
- [x] Tracing foundation
- [x] Per-request **root span** + per-stage child spans (partial coverage of ingest → serialization)
- [x] Per-stage spans on the **storage read path** (`storage.{range,value_range,point}.read`
- [x] Arrow/Parquet decode on ingest
- [ ] Remaining per-stage spans
- [x] **OTLP trace export** (pairs with `/metrics`)
- [x] **OTLP collector verification: delivery verified.**
- [x] **`Statement::n_change()` write accounting (Turso 0.6) in ingest/instrumentation spans**
- [ ] `/bench/runs/:id` endpoint

### Phase 4 — Hot path: physical types & Storage v2 · *High*

- [x] **4.1 Physical numeric encodings**
  - [x] Integer-epoch `TimeUnit` (s/ms/µs/ns) with lossless delta + delta-of-delta transforms, zig-zag + LEB128 varint estimate, RLE, and `best_estimated_bytes` selector
  - [x] Fixed-width bit-packing codec
  - [x] Monotonic-order enforcement
  - [ ] Explicit tz + leap-second policy
  - [x] Out-of-order **reconciliation** (Phase 4.6)
- [x] **4.3 Columnar segment store**
  - [x] Segment-level time/value pruning (`prune_by_time`/`prune_by_value`, conservative `may_contain_value`)
  - [x] Quality pruning (`prune_present_by_time`, all-null segment/page skipping)
  - [x] Intra-segment page skipping (`prune_pages_by_time`, decode only surviving pages)
  - [ ] Tag pruning *(blocked on B-tags — per-measurement tags/labels don't exist yet)*
- [x] **4.5 Arrow-compatible arrays**
- [ ] **4.6 Correctness semantics**
  - [x] Out-of-order **detection + enforcement** (shipped in 4.2)
  - [x] Out-of-order **reconciliation — in-place per-segment sort** (first slice):
  - [x] Out-of-order **reconciliation — cross-segment merge** (baseline)
  - [x] Out-of-order **reconciliation — split-not-rewrite optimization**
  - [ ] Out-of-order **reconciliation — configurable upsert keys + skip-identical**:
  - [x] **Threshold-triggered background reconciliation**
  - [x] **Hot/cold split in the background reconcile**
  - [x] Read-planner

### Phase 5 — GPU interpolation flagship · *High (parallel w/ Phase 4)*

- [x] Interpolation/extrapolation capability
- [x] GPU infra
- [ ] 5.1 Command batching
- [ ] 5.2 True async GPU handles (non-blocking; CPU parse/read overlaps GPU)
- [ ] 5.3 CPU/GPU overlap (chunked read → decode → upload → kernel → stream output)
- [ ] 5.4 Hardware auto-tuning (calibrate CPU/GPU throughput, transfer cost, break-even sizes; auto-select)
- [ ] 5.5 GPU economics (cloud cost, CPU-only fallback, utilization/contention/cold-start, NVIDIA/AMD/Intel/Apple portability)
- [ ] 5.6 wgpu/WGSL portability & determinism (conformance matrix, numerical drift, published tolerances)
- [ ] 5.7 Multi-GPU *(deferred until single-GPU wins are proven)*
- [ ] Memory-mapped GPU I/O *(only after transfer bottlenecks are measured)*
- [ ] End-to-end GPU interpolation benchmark (storage read → … → API serialization, with p95)
  - [x] **DONE (2026-10-09) — the storage half:**
  - [x] **MEASURED + DONE (2026-10-09) — where the end-to-end time goes, and the `f64` egress.**
  - [ ] **FOUND (2026-10-11, runtime) — `POST /api/v1/downsample` rejects the documented multi-word
  - [ ] **FOUND (2026-10-09) — the compute endpoints widen JSON `f64` values with `BigDecimal::from_f64`** (`weft-server/src/downsample.rs:138` …
  - [ ] **NEXT — the compute endpoints pay the same tax:**
  - [ ] **Reconcile with EVI-10A** (2026-10-19: the API now has an input cap, `interpolate_range::MAX_KNOTS`, a `pub` const the review may keep, turn into an option or drop)
  - [ ] **NEXT (post-1.0 per decision 18) — the HTTP endpoint:**
- [x] **DONE (2026-10-09) — GPU memory-stability + pool-eviction benchmark** (`weft-bench --gpu-memory`, `gpu_pool` in schema v22)
  - [ ] **NEXT — device memory, not just pool bytes:**

### Phase 6 — Compression v2 · *High*

- [ ] **6.1 Lossless typed codecs**
  - [ ] **Codec patent/licence gate (owner decisions 2026-10-08).**
    - [x] **Elf, FIRE and the advisory f64 codecs: gated.**
    - [x] **Bit-sliced value codec: gated pending counsel.**
    - [ ] **Default FOR codec and the Tiger Data family: with counsel.**
    - [ ] **The 2026-10-08 routine's advisory codecs: not yet reviewed, and ungated.**
    - [ ] **SAP family: review before the Phase 6.2 design.**
    - [ ] **Possible future: a patent-clear codec in a new crate.**
    - [x] **Attribution for adapted code.**
    - [ ] **Package licence metadata: mitigated by the relicense; the expression is with
  - [x] **Gorilla + RLE realized on disk**
  - [x] **Evaluated** a Gorilla-style variable-length second-difference estimate
  - [x] **Adopt-or-drop decision for the Gorilla codec: ADOPTED.**
  - [x] **Evaluated + ADOPTED dynamic (per-block adaptive) bit packing.**
  - [x] **Frame-of-Reference (FOR) per-block codec — prototyped + benchmarked (advisory).**
  - [x] **FOR adopt-or-drop — DECIDED (owner sign-off): ADOPTED for the value column,
  - [x] **Sprintz-style FIRE predictor + zero-RLE (advisory):**
  - [x] **FIRE adopt-or-drop — DECIDED (2026-07-16): DROPPED.**
  - [ ] **Evaluate Pcodec as an integer/f64 codec + bench baseline — now with the upstream numbers
    - [ ] **SCOPE CORRECTION (research 2026-09-23): evaluate pco for the COLD TIER and as a
    - [ ] **The real prize in pco may be its MODE DETECTION, not its format.**
    - [x] **DONE (2026-10-08) — `pco` reached 1.0 (1.0.3, 2026-08-01); `pco =1.0.4` is pinned as a
    - [x] **Measured (2026-10-08), value column, per column as asked.**
    - [x] **DONE (2026-10-08) — timestamp half measured** (`benches/timestamp_vs_pco.rs`, 1 Mi each,
    - [x] **DONE (2026-10-08, advisory) — factor a timestamp column's common multiple (pco's `IntMult`) before DoD
      - [ ] **NEXT — realize the common-multiple factor as a timestamp codec flag (owner sign-off:
    - [x] **DONE (2026-10-08, advisory) — first-order delta + FOR as a timestamp candidate beside DoD.**
  - [x] **Scaled-int value bit-pack codec — realized on disk.**
  - [x] **Value-column realized-bytes accuracy — headline FLIPPED (owner sign-off,
  - [x] **Per-block adaptive bit-pack for the VALUE column — realized on disk.**
  - [x] **f64 value-column codec — Gorilla + Chimp XOR codecs + best-of selector (advisory).**
  - [x] **Faithful Chimp128 (128-value reference window) — shipped.**
  - [x] **Elf (erasing-based) f64 codec — shipped (conservative advisory variant).**
  - [ ] **f64-codec adopt: target ALP, not Chimp128/Elf — the state of the art.**
  - [ ] **ALP now has a FROZEN wire layout to mirror — do not invent a container (research
  - [x] **DONE (2026-10-08) — Use the `alp` crate to get the ALP adopt benchmarked, rather than hand-rolling it first.**
    - [x] Shipped `weft-physical-type/benches/alp_vs_f64_codecs.rs` (`alp =0.0.4` + `fastlanes =0.7.2`,
    - [ ] **ALP adopt as `VAL_CODEC_ALP` — owner sign-off (headline change).**
    - [x] **DONE (2026-10-08, advisory) — per-vector scale is the lever the realized exact path is missing.**
    - [x] **DECIDED (2026-10-08) — per-block wins on BTC.**
    - [x] **Prior art for the decimal-exponent FOR: BtrBlocks' Pseudodecimal Encoding (SIGMOD'23)
    - [ ] **NEXT — realize the decimal-exponent FOR as `VAL_CODEC_DFOR` (owner sign-off: headline
      - [x] **DONE (2026-10-08)** surfaced as `storage.advisory_dfor_value_bytes` in the `weft-bench`
    - [x] **DONE (2026-10-08) — the scalar FOR/blocked decode is the read-path bottleneck at wide widths.**
    - [ ] **Re-decide the transposed layout now that the linear baseline is honest — RECOMMEND
    - [x] **DONE (2026-10-08) — The f64 codecs (Gorilla/Chimp/Chimp128/Elf) still read with their own per-bit loops**
    - [x] **DONE (2026-10-08) — the ~2.4 ms "frame parse" floor under every read was the CRC.**
    - [ ] **Next read-path floor: every point read still CRCs the whole frame** (real-data evidence
  - [ ] **Set the ALP acceptance bar from upstream's own ablation, and be willing to DECLINE.**
  - [ ] **GPU-decode ALP — retarget to G-ALP (DaMoN'25), and decide the exception layout BEFORE the
  - [ ] **Fuse decompression INTO the GPU interpolation kernel — with a number behind the wedge.**
  - [x] **FastLanes "Unified Transposed Layout" for the bit-pack codecs (decode-speed
  - [x] **Cascading (recursive) codec composition — delta→best-packer chain: advisory + on disk.**
  - [ ] **Evaluate Vortex as a columnar interchange + benchmark reference (Phase 1/4):**
- [ ] **6.2 Model-based compression** (leverages WeftDB's spline DNA, NeaTS-like)
- [ ] **6.3 Late/compressed-domain execution**
- [ ] **6.4 Quality benchmarks**

### Phase 7 — Online perf, durability, correctness · *High for beta*

- [ ] **7.1 Online ingest**
- [ ] **7.2 WAL & crash consistency.**
  - [x] **S1** Preserve legacy MVCC logs and drop no-op sync pragmas *(~2.5 h; after none)*
  - [x] **S2** Durable I/O layer, fault points and power-cut simulator *(~8 h; after none)*
  - [x] **S3** Storage-v2 open hardening
  - [x] **S4** Store-wide sweep isolation *(~4 h; after none)*
  - [x] **S5** Backup directory atomic publish, manifest, retention and drill hygiene *(~7 h; after S2)*
  - [x] **S6** Schema v2, IndexTxn, write poison, root-relative frame path resolution *(~7 h; after S3)*
  - [x] **S7** Persistent per-aspect id allocator, per-aspect commit/maint locks, plain INSERT for seals *(~6 h; after S6)*
  - [x] **S8** Write-once maintenance I
  - [x] **S9** Write-once maintenance II
  - [ ] **S10** Write-once durable seal with id assigned at commit *(~8 h; after S9)*
  - [ ] **S11** Rollup into segment_index.db, folded in the seal and swap transactions *(~7 h; after S10)*
  - [ ] **S12** Startup recovery, quarantine with TTL, fsck endpoint, /ready report *(~8 h; after S11)*
  - [ ] **S13** Idempotent atomic ingest entry point (library) *(~7 h; after S12)*
  - [ ] **S14** HTTP ingest wiring
  - [ ] **S15** Partial sidecar v4
  - [ ] **S16** Whole-store backup with hard-linked frames
  - [ ] **S17** Relaxed durability mode with synced_epoch watermark (optional for 1.0) *(~6 h; after S12)*
  - [x] **S18** Legacy rows-mode hygiene
  - [ ] **S19** Seal-backed legacy aspects
  - [ ] **S20** Change log replaces the per-timestamp unbatched queue for seal-backed aspects *(~7 h; after S19)*
  - [ ] **S21** Crash-matrix CI, subprocess SIGKILL soak, durability benchmark and ROADMAP re-baseline *(~7 h; after S14, S16)*
- [ ] **7.3 Corruption detection**
  - [x] Online control-plane backup over `VACUUM INTO` + per-database verification (see the Turso section below)
  - [x] Background backup daemon (`WEFT_BACKUP_INTERVAL_SECS`) with generated-snapshot retention (`WEFT_BACKUP_KEEP`)
  - [x] Concurrent-write-safe verification (`VerifyMode::SnapshotOnly`)
  - [x] **Restore + a backup/restore drill**
  - [x] **Restore drill at the API**
  - [ ] Restoring *into a chosen new root* over HTTP
  - [ ] Whole-store backup manifest (fold the `.weftseg` frames in beside the control plane)
  - [ ] Document RPO/RTO against the measured snapshot cadence
- [ ] **7.5 Compaction**
- [ ] **7.6 Quotas/limits**

### Phase 8 — Commercial hardening · *Required for paid beta*

- [ ] Security
- [ ] Compliance readiness
- [ ] Packaging
- [ ] Observability
- [ ] SDKs — Rust → Python → TypeScript → R/Arrow
- [ ] Licensing/legal
  - [x] **Licence: `MIT OR Apache-2.0`**, the Rust ecosystem's dual licence, for every WeftDB crate
  - [x] **Copyright holder: Justin Icenhour**, as an individual, and the only one
  - [x] **Contribution terms: DCO or CLA, the contributor's choice.**
    - [ ] **Enforce it (owner, once on main):**
  - [x] **kdb+/KDB-X: excluded from published benchmarks** unless KX consents in writing
  - [x] **Codec patents: gate the flagged opt-in codecs.**
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
- [ ] **7.4 Whole-store backup manifest**
- [ ] **7.4 Documented RPO/RTO** against the measured snapshot cadence
- [ ] **7.5 Compaction** with query consistency, cancellation, and resource limits
- [ ] **7.6 Quotas/limits**
- [ ] **Phase 7 acceptance run**

### Hot path & data model *(must land before the freeze)*

- [x] **splimes 1.0 is released**, and WeftDB depends on it
- [ ] **Measurement bulk ingest routed through the `.weftseg` seal**
- [ ] **B-tags**
- [x] **Control-plane engine decision**

### Security & packaging *(Phase 8 minimum)*

- [ ] TLS on `weft-server`
- [ ] API-key/token auth + basic RBAC + audit log
- [ ] Packaging
- [ ] Upgrade/rollback procedure, exercised against the previous release

### Release engineering

- [x] **Dependencies current and audited (2026-10-05)**
- [ ] **CI**
- [ ] **Public surface declared and frozen**
- [ ] **Compatibility tests**
- [ ] **Semver policy + `CHANGELOG.md`**, starting from the first tagged pre-release
- [ ] **Crate publishing split**
- [ ] **Panic audit on server paths**
- [ ] **Security process**
  - [x] `SECURITY.md` is on main
  - [x] `cargo deny check advisories bans licenses sources` runs in CI (the `deny` job of `ci.yml`), covering what `cargo audit` would
  - [ ] **Private vulnerability reporting is off**

### Claims & documentation

- [ ] Every performance claim in the README links to a benchmark artifact (the [governing rule](#weftdb-roadmap))
- [ ] First cross-engine report
- [ ] Honesty pages shipped (see [Commercial thesis](https://basicautomation.io/projects/weftdb/docs/commercial-thesis))
- [ ] Legal review of competitor benchmark-publication terms + codec licenses (see [Legal review](#legal-review))
- [ ] README *Project status* table updated to reflect 1.0 and the status badge changed from `pre-beta`

### Execution waves (W0–W15)

- [x] **W0 · Land in-flight work and lock the shapes S6/S7 depend on**
  - [x] **INT-0** Merge deps/splimes-1 → routine/dsp-2026-10-08 → legal → durability/base into main
  - [x] **D-S3** Storage-v2 open hardening
  - [x] **D-S5** Backup directory atomic publish, manifest, retention and drill hygiene, + **SEC-1** backup-label grammar
  - [x] **D-S18** Legacy rows-mode hygiene
  - [x] **D-S6** Schema v2, IndexTxn, write poison, root-relative frame paths
  - [x] **D-S7** Persistent per-aspect id allocator, commit/maint locks, plain INSERT for seals (+ `next_series_id`)
  - [x] **PAC-1a** Release pipeline
  - [x] **TAG-1** TagSet, SeriesKey and SeriesSelector with canonical form and frozen caps
  - [x] **Decision 1** Store shapes for S6/S7
  - [x] Turn on GitHub private vulnerability reporting (Settings → Security) *(owner)*
- [ ] **W1 · Baseline release, server split, goldens**
  - [x] **PAC-4** Cut the v0.1.0 baseline release (GitHub release only)
  - [x] → **X-FX** One fixture set under `compat/fixtures/v0.1.0/`, written by the v0.1.0 binary (absorbs PAC-FX, FRE-2)
  - [x] Delete the stale draft prerelease `v0.1.0-preview.1` *(owner)*
  - [x] Store lane
  - [x] → **D-S7** (see W0)
  - [x] **X-SPLIT-SRV** Split manage.rs into declare/ingest/maintain/backup_routes, and main.rs into startup phases
  - [ ] **FRE-1** Release truth, per-crate versions, single edition, publish split, package hygiene
  - [ ] **FRE-3** Golden frames, golden sidecars and persisted-encoding goldens (gated codecs included)
  - [x] **PAC-1b** Stripped dist profile and a third-party license bundle (absorbs EVI-20)
  - [x] **PAC-5a** Configuration registry skeleton
  - [ ] **TAG-3** Open `ValueKind` in weftdb, provenance scope
  - [ ] **EVI-1** Claims hygiene
  - [ ] **EVI-3** Run manifest (schema v23: the 2026-10-09 routine took v20–v22, so EVI-3 takes the next free number per the plan's rule)
  - [ ] **EVI-9** Block missingness and scale presets
  - [ ] **SOA-1** Soak harness skeleton
- [ ] **W2 · Error contract, frame format, portable builds**
  - [x] Store lane
  - [x] → **D-S9** Write-once maintenance II
  - [x] **X-ERR** One error envelope and code table in `error.rs` (absorbs SEC-2, FRE-6a, ROB-1, TAG-0's envelope)
  - [x] → **X-NAME** `aspect_name::encode` and `validate_new` at HTTP declare (absorbs FRE-7a, SEC-0)
  - [x] → **ROB-2** Storage-engine panic fail-stop
  - [x] **PAC-3** Library crate hygiene
  - [x] L-FMT: **X-FRAME** Frame registry, v7 envelope, `read_frame*`/`describe_frame`, `FrameFault` classes (absorbs FRE-4, TAG-2)
  - [ ] Legacy lane
  - [ ] **PAC-2** Portable Linux binaries on a glibc 2.28 baseline
  - [ ] **EVI-5** Nightly benchmark-regression workflow and `weft-bench compare`
  - [ ] **EVI-2** Claims registry and markdown claim/link lint
  - [ ] → **EVI-4** Artifact pipeline
  - [ ] **EVI-6** Ingest workload over SegmentStore and the UCI corpus loader
  - [ ] **EVI-8A** Benchmark adapter v2 I
  - [ ] **SOA-2** Soak preflight, device resolution, run metadata and secret redaction
  - [ ] **SOA-3** Open-loop scheduler, HDR interval logs, harness self-monitoring and null server
  - [ ] **SOA-4** Deterministic workload model, lanes, format-exact value grids and body encoders
- [ ] **W3 · Gate, serve path, input domain**
  - [ ] Store lane
  - [ ] → **EVI-10A** Library stored-range interpolation with input caps (a child module, one `mod` line)
  - [ ] → **D-S11** Rollup into segment_index.db, folded in the seal and swap transactions
  - [ ] **SEC-3** Sealed route registry (`RouteSpec`), frozen scope table and fail-closed gate
  - [ ] **SEC-4** One HTTP/1.1 serve path with pre-auth listener limits (absorbs ROB-23's caps)
  - [ ] **ROB-6** Bounded decimals
  - [ ] **EVI-8B** Benchmark adapter v2 II
  - [ ] **SOA-5** Soak ingest client
  - [ ] **SOA-12** Linux system samplers, contamination flags and the strict-mode flush check
- [ ] **W4 · Auth, config, ILP fix**
  - [ ] Store lane
  - [ ] → **D-S13** Idempotent atomic ingest entry point (library) (+ `groups`, ledger `series_ids`, typed errors)
  - [ ] **SEC-5** TLS listener (rustls with ring) on the shared serve path
  - [ ] **SEC-6** API keys, Bearer authentication, keys file and keygen
  - [ ] **ROB-3** Panic containment, daemon supervision, Daemons registry, `weftdb::exec`
  - [ ] **PAC-5b** Strict server configuration and store filesystem check (the bind guard moves to SEC-8)
  - [ ] **TAG-0** Fix ILP series merging
  - [ ] L-FMT: **ROB-8** Frame decode hardening
  - [ ] **EVI-11** Stored WeftDB benchmark adapter
  - [ ] **SOA-6** Soak oracle core
- [ ] **W5 · Ingest wiring, whole-store backup, legacy seal**
  - [ ] Store lane
  - [ ] → **D-S15** Partial sidecar v4
  - [ ] → **D-S19** Seal-backed legacy aspects
  - [ ] L-INGEST
  - [ ] **ROB-4** Enforced no-panic lint (including asserts) and the weft-arrow Result API
  - [ ] **ROB-10** Admission I
  - [ ] **SEC-8** Startup ordering, exposure matrix, auth-off guard, metrics auth
  - [ ] → **SEC-10** Exposure and secrets hygiene
  - [ ] **FRE-5** Compute vocabulary, exact-case tokens and no silent narrowing
  - [ ] **FRE-7b** Immutable declarations, scale rules and schema read-back
  - [ ] **EVI-13** DuckDB adapter I
  - [ ] **EVI-15R** Report renderer with honesty-section generation
  - [ ] **EVI-16** Honesty pages I
  - [ ] **SOA-7** Window digests and the oracle self-test
  - [ ] **SOA-8** Failure deck, phases, recovery windows, SIGTERM verdict and the maintenance driver
- [ ] **W6 · Durability CI, store split, scheduler**
  - [ ] **D-S21** Crash-matrix CI, subprocess SIGKILL soak, durability benchmark and re-baseline, with **SOA-9** (`crash_kill9`)
  - [ ] Run D-S21's privileged power-loss validation (sudo: dm-log-writes or a QEMU hard reset) *(owner)*
  - [ ] Store: **X-SPLIT-STORE** Split segment_store.rs into `open`/`read`/`seal`/`maint`/`stats` submodules
  - [ ] → **TAG-4** weftdb read sites on the frame-reader API with typed errors
  - [ ] **FRE-6b** Strict request parsing and caller updates
  - [ ] **SEC-7** Key reload
  - [ ] **SEC-9** Audit
  - [ ] **SEC-11** Aspect-name traversal HTTP regression tests
  - [ ] **PAC-7** Operator CLI, exit-code contract and store identity at startup
  - [ ] → **ROB-11** Admission II
  - [ ] **ROB-19** Maintenance scheduler, compaction on by default, per-aspect circuit breaker
  - [ ] **SOA-10** Derived-read checks
  - [ ] **SOA-14** Server hooks
  - [ ] **EVI-14** DuckDB adapter II
  - [ ] **EVI-18** Backup health metrics and catch-up-free ticking
  - [ ] **EVI-25** Backup, restore and recovery cost measurement (RTO inputs and backup duration)
- [ ] **W7 · Post-split lanes open; security and packaging close**
  - [ ] open lane
  - [ ] → **FRE-12b** Scope pin, `GET /api/v1/version`, incompatible-store mapping and exit code
  - [ ] seal lane
  - [ ] read lane
  - [ ] → **ROB-13** Bounded stored downsample and point lookups
  - [ ] **ROB-7** Ingest decode bounds
  - [ ] **ROB-16** GPU configuration, compute limits, interpolation provenance and metrics
  - [ ] **ROB-23r** Probe-only admin listener
  - [ ] **PAC-13** Offline restore CLI over D-S16 (`weft-server restore`)
  - [ ] **ROB-5** Legacy queue-consumer panics
  - [ ] **SEC-13** Security acceptance
  - [ ] **PAC-9** Docker image and container smoke test (with a mounted keys file)
  - [ ] **PAC-11** systemd unit, user and env files, and host-install CI
  - [ ] **EVI-15** First cross-engine report
  - [ ] Write or approve the cross-engine report narrative *(owner)*
  - [ ] **EVI-19** Documented RPO/RTO
  - [ ] **SOA-11** Soak backup lane, online prefix check, final restore verification, RPO/RTO
- [ ] **W8 · Tags core, memory budget**
  - [ ] open lane
  - [ ] → **TAG-9** Frame error classes in recovery
  - [ ] seal lane
  - [ ] → **ROB-17** Disk guard, headroom API and space gauges
  - [ ] maint lane
  - [ ] read lane
  - [ ] **ROB-14** Cgroup-aware memory budget with per-request reservations
  - [ ] **FRE-8b** Stored-value text and one float-input rule across every ingest format
  - [ ] **FRE-9a** OpenAPI generated from code
  - [ ] **ROB-9** Mutation harness for frames, sidecars and Parquet
  - [ ] **SEC-14** Security documentation, config schema and upgrade notes
  - [ ] **PAC-10** Publish the multi-arch image (only after API auth)
  - [ ] Make the ghcr.io weftdb package public after PAC-10's first push *(owner)*
  - [ ] **EVI-17** Honesty pages II
  - [ ] Write or approve the honesty-page narrative *(owner)*
  - [ ] **FRE-13** Layout-1 to layout-2 upgrade test
  - [ ] **SOA-13** SoakReport, SLO evaluator R1–R8, coverage floors, honesty section
  - [ ] → **SOA-15** soak.yml nightly workflow, TTL lane and nightly-to-nightly comparison
- [ ] **W9 · Read semantics, limits, upgrade legs**
  - [ ] read lane
  - [ ] maint lane
  - [ ] open lane
  - [ ] L-INGEST
  - [ ] storage read handlers lane
  - [ ] **FRE-10a** Contract tests
  - [ ] **PAC-14a** UPGRADING.md and compat legs A (upgrade) and C (rollback after writes) (absorbs FRE-19)
  - [ ] **SOA-16** Soak upgrade lane
- [ ] **W10 · Tag gate flip, compaction closes**
  - [ ] **TAG-11** HTTP ingest with tags and the tagged-write gate flip
  - [ ] **ROB-21** Maintenance cancellation, RAII cleanup guards, minimal maintenance metrics
  - [ ] read lane
  - [ ] **FRE-15** STABILITY.md, semver policy and the release-criteria wording
  - [ ] **PAC-15** Operator documentation
- [ ] **W11 · Contract candidate, beta tag, calibration**
  - [ ] Move the T7 reference drive (`/mnt/weftbench`) to a 10 Gbps port before calibration *(owner)*
  - [ ] **FRE-9b** OpenAPI
    - [ ] Extend the release tag grammar to `vX.Y.Z-beta.N` (`TAG_RE` in `scripts/release/verify.sh` accepts only `vX.Y.Z` and `vX.Y.Z-rc.N` today, so a beta …
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

- [ ] **B-tags** *(Phase 2/4 · absent)*
- [ ] **B-conn** *(Phase 2/7 · absent)*
- [ ] **B-ilp** *(Phase 2 · partial)*
- [ ] **B-rest** *(Phase 2 · partial)*
- [ ] **B-poll / B-retry / B-register** *(Phase 7 · absent)*
- [x] **B-interp** *(Phase 5)*
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

- [x] Migration 0.4 → 0.6
- [ ] Lean on production MVCC concurrent writes for concurrent ingest + catalog updates under load *(Phase 7)*
- [ ] Encryption at rest (AEAD pager) for catalog/metadata/pipeline-state DBs *(Phase 8)*
- [x] `VACUUM INTO 'file'` for online, consistent control-plane backup/snapshot *(Phase 7.4)*
- [x] **Backup 7.4 residue (a) — concurrent-write-safe verify: shipped (2026-07-23).**
- [x] **Backup 7.4 residue (c) — background backup daemon: shipped (2026-07-23).**
- [ ] **Backup 7.4 residue (b) — incremental / WAL-streaming backup** beyond the full `VACUUM INTO`
- [ ] **Backup 7.4 residue (d) — whole-store backup manifest**
- [x] **Backup cost: MEASURED (2026-09-23), and the diagnosis on record was wrong.**
- [x] **Snapshot dirs no longer accumulate empty `.db-log`/`.db-wal` sidecars**
- [ ] **Backup cost residue (a) — the root cause is documented upstream, and it is per-call
- [ ] **Backup cost residue (b) — (i) MEASURED 2026-10-08: `synchronous=OFF` is NOT a material win.**
- [ ] **Backup cost residue (c) — DO NOT reach for Turso's experimental passive checkpoint.**
- [ ] **Backup 7.4 residue (b) is UPSTREAM-GATED, not an open design question (research 2026-09-23):**
- [ ] **Backup cost residue (d) — evaluate an `sqlite3_rsync`-style page-delta replica.**
- [ ] **Confirm the snapshot verifier does not depend on MVCC-broken pragmas.**
- [x] **Upgraded to `turso` 0.8.1 (2026-10-05).**
- [x] ~~**Evaluate the `turso` 0.7 upgrade**~~
- [ ] Triggers (`BEFORE/AFTER/INSTEAD OF` + `WHEN`)
- [x] `Statement::n_change()` affected-row accounting
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

- [ ] Review each competitor's benchmark-publication terms (kdb+ especially)
  - [x] **kdb+/KDB-X (owner decision 2026-10-08): excluded from every published benchmark unless KX consents in writing.**
  - [x] **WeftDB's own terms (2026-10-08):**
  - [x] **Codec patents (2026-10-08):**
  - [ ] **Open with counsel:**

## Immediate next actions

- [ ] **START HERE (filed 2026-10-11 for the next run).**
- [ ] **Research (2026-10-11) — the exact TWA is now a measurable differentiator.**
- [ ] **START HERE (filed 2026-10-09 for the next run).**
- [ ] **(2026-10-08 list.)** The 2026-10-08 run (PR `routine/dsp-2026-10-08`)
- [ ] **(2026-09-23 list; item 3 shipped 2026-10-08.)** The 2026-09-23 run closed both cheap backup
- [ ] **POSITIONING ALERT — WeftDB's precision wedge now has a direct competitor (research 2026-09-23).**
  - [x] **MEASURED (2026-10-08) — WeftDB's own decimal-vs-f64 factor is ~43×, not ~2×.**
  - [x] **DONE (2026-10-08) — `weft_reduce::reduce_scaled`**, the integer-native streaming reduction
  - [x] **DONE (2026-10-08) — the `avg` division floor.**
  - [x] **DONE (2026-10-08) — wired into `SegmentStore::downsample_range`:**
  - [x] **DONE (2026-10-08) — `reduce_partial_scaled` accepts `sketch_p*`**, feeding each bucket's
  - [x] **MEASURED (2026-10-08) — per-segment end to end** (`decimal_tax` `segment_downsample_*`, one real
  - [x] **DONE (2026-10-08) — `BucketAcc::finish` computes `avg` through `avg_of_sum`**, the
  - [ ] **NEXT — the remaining 6.3×:**
    - [x] **DONE (2026-10-09) — no per-sample bucket indexing on time-ordered input.**
    - [x] **DONE (2026-10-09, sub-items below) — the `avg` division was the biggest piece of `reduce_scaled`:**
      - [x] **TRIED (2026-10-09), NO WIN — 19 digits per `u128` division.**
      - [x] **DONE (2026-10-09) — one `BigUint` multiply + one division per non-terminating
    - [x] **DONE (2026-10-11) — per-bucket bookkeeping off the integer path.**
    - [x] **DONE (2026-10-11) — the same for `reduce` on `BigDecimal` points**
    - [x] **DONE (2026-10-11) — time-weighted averages on integers.**
  - [ ] **NEXT — the sidecar build.**
  - [ ] *(original item)* **a `ScaledI64`-native reduction fast path in `weft-reduce`:**
- [ ] **External-engine adapter fairness — three concrete rules from the 2026 landscape.**
- [ ] **Adopt the TSM-Bench QUERY SPEC (not the repo) as Weft-Bench's external comparison workload.**
- [ ] **BENCHMARK HYGIENE — the synthetic ingest corpus flatters WeftDB, by a lot (measured 2026-07-23).**
  - [ ] Audit the other `weft-bench` workloads' generators for the same class of flattery (the shape
    - [x] **DONE (2026-10-08) — the compression workload runs on a real corpus:**
    - [x] **DONE (2026-10-08) — `point_lookup` on a real corpus:**
    - [x] **DONE (2026-10-08) — `range_fetch` on a real corpus:**
    - [x] **DONE (2026-10-08) — `downsample` on a real series:**
    - [x] **DONE (2026-10-09) — the harness times the integer path too:**
    - [x] **DONE (2026-10-09) — the interpolation generator: pessimistic at scale, now a knob.**
    - [x] **DONE (2026-10-11) — the storage generators are NOT representative; they flatter.**
    - [ ] **NEXT — a representative storage generator, or real-corpus defaults.**
    - [x] **DONE (2026-10-09) — the downsample generator emits realistic decimals:**
    - [ ] **FOUND (2026-10-09) — `sketch_p99`'s published ~2.6× speed-up over exact `p99` was the
      - [x] **DONE (2026-10-09) — exact percentiles select instead of clone-and-sort.**
      - [x] **DONE (2026-10-09) — the sketch's per-sample `BigDecimal → f64`.**
      - [x] **DONE (2026-10-09) — the sketch skips `ln` for a value inside the last sample's bucket**
      - [x] **TRIED (2026-10-11), NO WIN — a dense bin store.**
      - [ ] **Research (2026-10-09) — a cheaper sketch index, if the sketch format is ever revised.**
      - [ ] **Positioning (2026-10-09) — exact percentiles are a differentiator against QuestDB.**
      - [x] **DONE (2026-10-09) — the scaled partial serves exact percentiles and TWA.**
      - [x] **DONE (2026-10-09) — `reduce`/`reduce_partial` take the integer path themselves**
      - [ ] **NEXT — select on the mantissas when a partial is not merged.**
- [x] **DONE (2026-07-20) — BUG ROOT-CAUSED + FIXED: the "flaky GPU interpolation tests" were never a
- [x] **DONE (2026-07-20) — BLOCKER CLEARED: the `gpu_integration_tests` rustc ICE was an
- [x] **DONE (2026-07-20) — the LNK1102 link OOM is FIXED at the root: `[profile.test] debug = 1`.**
- [x] **DONE (2026-07-20) — `cargo test --workspace` COMPLETES for the first time: 969 passed, 0
- [x] **DONE (2026-07-23) — the BTC bulk-load test is RUNNABLE, not merely skippable.**
- [x] **ANSWERED (2026-09-23) — `SKIP_SLOW_TESTS` cannot be dropped workspace-wide yet, and the
- [ ] **The three unguarded `weft-orchestration` failures are latent bugs, not flakes — fix them
  - [x] **Re-checked 2026-10-08:**
- [ ] **Re-measured 2026-10-09 — still unbounded.**
- [ ] **Bound the seven slow `weft-orchestration` tests the way `db_tests` was bounded.**
- [ ] **Historical note — the LNK1102 research that produced the `[profile.test] debug = 1` fix:**
- [ ] **Guard the `#[serial]`-above-`#[test]` ordering (research 2026-07-20):**
- [x] **DONE (2026-07-14, capstone of the 2026-07-13 read-path arc) — `weft-bench` `point_lookup` workload
- [x] **NEW — cross-engine point-lookup comparison** is the residue
- [x] **`weft-reduce` crate + downsample reduction (2026-07-14):**
- [x] **DONE (2026-07-16) — approximate mergeable quantiles (DDSketch):**
- [ ] **NEXT — UDDSketch (uniform guarantee under collapse):**
- [x] **DONE (2026-07-16) — sketch rank-convention divergence REMOVED.**
- [x] **DONE (2026-07-16) — the mergeable-reduction primitive exists.**
- [x] **DONE (2026-07-16) — `PartialReduction` has a consumer + a number.**
- [x] **DONE (2026-07-16) — cross-segment downsample, store layer.**
- [x] **DONE (2026-07-17) — the cross-segment downsample is exposed at the API.**
- [x] **DONE (2026-07-17) — `downsample_range` reduces its segments concurrently — measured 4.7×.**
- [x] **DONE (2026-07-18) — materialize per-segment partials at seal time (the highest-value
  - [x] **(1) per-segment `.weftpart` sidecar at a declared base resolution**
  - [x] **(2) `downsample_range` merges sidecars in place of a decode** when every requested reduction
  - [x] **re-bucketing a fine base to a coarser resolution**
  - [x] **sidecar consistency across rewrites**
- [x] **DONE (2026-07-19) — (3b) hierarchy of materialized tiers.**
- [x] **DONE (2026-07-19) — (3a) materialization watermark: satisfied by construction.**
- [x] **DONE (2026-07-20) — (3c)(ii) the tier win SCALES with base-buckets-per-segment, but sub-linearly.**
- [ ] **NEXT — (3c) materialized tiers, remaining:**
- [ ] **Positioning — WeftDB's `.weftpart` sidecar IS the "incremental materialized view", and mergeable
- [ ] **NEXT — materialize the sidecar at a coarser DEFAULT + adopt sketches:**
- [x] **DONE (2026-07-19) — per-segment overhead knee measured + a size-targeted compaction shipped.**
- [x] **DONE (2026-07-19) — size-targeted compaction wired into the reconcile daemon + a manual
- [x] **DONE (2026-07-19) — fragmentation gate for the compaction daemon.**
- [ ] **NEXT — default target for the compaction daemon:**
- [x] **DONE (2026-07-16) — linear (trapezoidal) TWA method beside the shipped LOCF weighting:**
- [x] **DONE (2026-07-16) — TWA residue: the *last-point-to-bucket-end* weighting.**
- [x] **DONE (2026-07-16) — server ILP `parse_aggregation_token` adopts
- [x] Create `weft-bench` as a first-class workspace member
- [x] Define the first benchmark profile
- [x] WeftDB adapter + portable baselines (linear class-C, forward-fill class-B) + accuracy scoring + shape-selectable ground truth
- [ ] Add the **DuckDB** adapter
  - [ ] **Research (2026-10-09) — DuckDB has a native linear-interpolation arm:**
- [ ] Add ClickHouse, InfluxDB 3, QuestDB, TimescaleDB adapters
- [x] Implement InfluxDB Line Protocol ingest (shared `weft-line-protocol` crate; bench + server wired end-to-end)
- [ ] Add end-to-end timing spans *(harness-level spans shipped; per-pipeline-stage spans = Phase 3 tracing item)*
- [x] **Cross-segment out-of-order merge (Phase 4.6):**
- [x] **Hot/cold split in the background reconcile (Phase 4.6):**
- [x] **Split-not-rewrite optimization (Phase 4.6):**
- [x] **Per-stage tracing child spans (Phase 3):**
- [x] **OTLP trace export (Phase 3):**
- [x] **OTLP collector verification: shipped.**
- [x] **Cursor paging (Phase 2 B-rest):**
- [x] **Gorilla-codec adopt-or-drop (Phase 6.1): ADOPTED + realized.**
- [x] **Scaled-int value bit-pack codec (Phase 4/6): shipped + realized on disk.**
- [x] **Per-block adaptive (dynamic) bit-pack timestamp codec (Phase 6.1): shipped.**
- [x] **`storage.ingest.parquet` stage span (Phase 3): shipped.**
- [x] **FOR (frame-of-reference) before bit-packing (Phase 6.1): prototyped + benchmarked
- [x] **Per-block adaptive (blocked) VALUE codec (Phase 6.1): shipped + realized on disk.**
- [x] **FOR adopt-or-drop (Phase 6.1): DECIDED — ADOPTED for the value column (owner
- [x] **FastLanes transposed bit-unpack (Phase 6.1, decode-speed): shipped (prototype).**
- [x] **Transposed-layout tile-level random access (Phase 6.1, decode-speed residue — first
- [x] **Transposed layout REALIZED on disk (Phase 6.1, decode-speed residue) — shipped 2026-09-23,
- [x] **Windowed value reads no longer re-walk the codec chain per row (found by review of the
- [x] **`read_value_range` cannot panic on a malformed block (found by the same review).**
- [x] **DONE (2026-10-08) — the transposed layout's real lever is a per-tile decode-count threshold, not the
- [ ] **NEXT — upstream FastLanes does NOT make the transposed layout its default either, and
- [x] **DONE (2026-10-08) — Cross-check WeftDB's hand-rolled bit-plane decoder against the `fastlanes` crate (0.7.2,
  - [x] Yardstick shipped (`benches/fastlanes_yardstick.rs`, bench-only pinned dev-dep)
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
- [x] **Headline bytes/point FLIPPED to the realized figure (Phase 4/6, owner sign-off,
- [x] **weft-bench parallel-test OOM — ROOT-CAUSED + FIXED (it was a logic bug, not
- [x] **f64 value-column codecs (Phase 6.1): Gorilla + Chimp + Chimp128 + Elf-style, best-of
- [x] **Sprintz FIRE forecaster + zero-RLE (Phase 6.1): shipped advisory + bench-surfaced (schema
- [x] **Cascading codec composition (Phase 6.1): advisory + on-disk descriptor shipped.**
- [ ] **Cascade adopt-or-drop into the DEFAULT selector — owner-gated (headline change, as FOR/ALP):**
- [x] **Block-level random access (Phase 6.1): shipped.**
- [x] **Streaming single-value point read wired into the segment/API point-read path (Phase 4/6):
- [x] **Streaming point read extended to paged segments (Phase 4/6): shipped.**
- [x] **Batch point lookup — amortize the timestamp decode across many instants (Phase 4/6):
- [x] **Batch point lookup exposed at the API (Phase 2/4): shipped.**
- [x] **Closed-form timestamp lookup for regular (constant-stride) columns (Phase 4/6): shipped.**
- [x] **DONE (2026-07-16) — block-random-access timestamp search for *irregular* sorted columns
- [x] **DONE (2026-07-16) — the checkpointed seal is REACHABLE (opt-in).**
- [ ] **NEXT — checkpoint index adopt-or-drop into the DEFAULT policy — owner-gated (bytes/point
- [x] **DONE (2026-07-16) — the `TS_CODEC_BLOCKED` override is QUANTIFIED, and it is NOT small.**
- [ ] **NEXT — a random-access-capable Gorilla/RLE would widen the checkpoint index's reach:**
- [x] **Windowed range read — decode only the row window for a regular block-coded segment (Phase
- [x] Add p50/p95/p99 + confidence-interval reporting
- [x] Add physical value types (`F64`, `ScaledI64`, `BigDecimalText` + three more)
- [x] Prototype columnar segment reads for one aspect type (`weftdb::SegmentStore`)
- [ ] Publish a methodology document **before** any performance claim
