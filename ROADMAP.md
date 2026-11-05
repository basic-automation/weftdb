# WeftDB Roadmap

## Honesty pages

- [ ] Ship honesty pages

## Phased roadmap (0–9)

### Phase 0 — Benchmark thesis & buyer wedge · *Immediate*

The performance thesis and the first benchmark-claim list are written, and the decision about what WeftDB is not chasing first is made; both live in the docs. Five open datasets now stand in for design partners, and the willingness-to-pay hypotheses are written down. They stay untested until buyer calls happen.

- [x] Identify 3–5 open datasets with interpolation-heavy workloads, standing in for design partners until Basic Automation is incorporated ([DESIGN-PARTNERS.md](DESIGN-PARTNERS.md); no partner contacted)
- [x] Document willingness-to-pay hypotheses ([WILLINGNESS-TO-PAY.md](WILLINGNESS-TO-PAY.md); every hypothesis untested)

### Phase 1 — Build `Weft-Bench` · *Highest priority*

`weft-bench` exists as a first-class workspace member with a CLI runner, a first workload profile, a WeftDB adapter over the vendor-neutral `SystemAdapter` trait, and portable baselines to compare against. Results are scored against analytic ground truth (RMSE, MAE, max error, bias) over selectable synthetic shapes, with latency statistics and a harness-level timing breakdown, and reported as JSON, self-contained HTML and Parquet. Each run records its hardware, disk and GPU/driver versions, and each result carries a storage estimate.

The storage, read and aggregation workloads shipped on 2026-07-14, and gap fill now runs on a real corpus. That run produced a negative result worth keeping: a spline fill does not beat linear on real prices. On real buoy temperature and pressure it wins only across short outages (#144). The compression workload's slow first repetition was explained as first-touch memory rather than lazy initialisation. Since 2026-11-02 every Phase 1 line is in place: six external engines, the fair protocol (raw samples, digests, pooled runs, sweeps, a seeded query mix, post-compaction reads, failure tests), the three interpolation classes, report surfaces for dashboards, and anti-Goodhart checks in CI (DECISIONS 2026-11-02).

- [x] **DONE (2026-11-02) — External-engine adapters** (#140): `weft-xbench` measures DuckDB, ClickHouse, TimescaleDB, QuestDB, InfluxDB 3 and IoTDB, each in up to three interpolation classes; VictoriaMetrics is not built (the plan conditions it on monitoring) and kdb+ is excluded by its licence
- [x] **DONE (2026-10-31) — TSBS-compatible comparison harness, on TSBS's data** (`weft-xbench --tsbs FILE`, #141): the `cpu-only` generator's output runs through every engine arm's load, interpolation and downsample phases, gated against the reference. TSBS's own query suite groups many series by tag, so it waits for tagged ingest and belongs to **TAG-12**.
- [x] **DONE (2026-10-09) — `gap_fill` workload** (`--gap-fill`)
  - [x] **DONE (2026-11-02) — `?fill=` on the downsample endpoints** (#142): body `fill` on `POST /api/v1/downsample`, `?fill=` on its ILP sibling and the stored-range endpoint, every output format; synthesized buckets carry `count == 0`, missing values are JSON-absent / CSV-empty / Arrow-null, grids past 10M buckets are a `400`
  - [x] **DONE (2026-11-02) — arbitrary bucket widths** (`5s`, `15m`, #143): `weft_reduce::BucketWidth`, the unit's partials merged by `PartialReduction::coarsen` (exact for every reduction), `SegmentStore::downsample_range_width`; the endpoints take `width` / `?resolution=15m`. Additive, so no API freeze decision was needed (DECISIONS 2026-11-02)
  - [x] **DONE (2026-11-01) — a real smooth-signal corpus for the spline-fill claim** (#144): NOAA NDBC buoy temperature and pressure (`scripts/bench/ndbc_to_csv.py`); cubic beats linear on 7 of 9 series at a two-hour mean outage and loses on 8 of 9 at a day
- [x] Remaining workloads
  - [x] **DONE (2026-11-01) — bulk-ingest growth curves** (`weft-bench --ingest --ingest-growth 1M,10M,100M,1B`, #145): 64k-row batches hold 3.9M → 3.2M rows/s to 1B; 1,024-row batches fall 141k → 107k rows/s by 100M as segments pile up
  - [x] **DONE (2026-11-01) — online ingest + query** (`weft-bench --ingest --ingest-online READERS`, #145): at 64k-row batches 100M rows ingest at 6.4M → 5.6M rows/s with 1 → 8 readers, every query exact; found reads costing ~1.8 µs per stored segment and, at 1,024-row batches, most concurrent reads refused with `database is locked`
  - [x] **DONE (2026-11-01) — compressed query** (`weft-bench --ingest --compressed-query QUERIES`, #145): in-store downsample is 2.2–2.9× faster than decode-then-reduce for a year window and equal below a day, where the per-segment prune cost sets the floor; every answer exact
  - [x] **DONE (2026-11-02) — analytics pipeline** (`weft-bench --ingest --analytics-pipeline ROWS`, #145): on UCI at 60-row batches every stage is timed and the detectors find exactly the raw extremes at 10k, 30k and 100k rows; preparation dominates and grows faster than the rows (82 s → 394 s → 1,290 s). Found `analyze_range` resampling off its step grid past 50k points, fixed
    - [x] **DONE (2026-11-03) — preparation cost** (#505): profiled at 10k rows; the incremental queue build interpolated and committed each 60-point window alone (~300 s of ~420 s). It now interpolates each run of windows once and queues 500 batches to a transaction: `prepare_data` 204 s. What is left is 100-digit BigDecimal arithmetic, in `splimes`' interpolation (~41 s) and `Batch::process` (~120 s), whose precision the stored values depend on
    - [x] **DONE (2026-11-03) — a chunk reads only one step past its edges** (#506): a chunk now widens its read until it holds the knots on both sides, so a gap longer than a chunk interpolates between its own brackets
- [x] **DONE (2026-10-09) — first-rep/warm separation** (`BenchResult.cold_warm`, schema v21, every workload; console + Parquet)
  - [x] **DONE (2026-10-31) — OS-cold and post-restart runs** (`weft-xbench --cold-hook CMD --cold-reps N`, #146): each read phase also runs with the page cache dropped and after an engine restart, every rep gated; schema v4 `warmth`
- [x] **DONE (2026-11-02) — Fair-protocol depth (Phase 1.1)** (#147)
  - [x] **DONE (2026-11-02) — raw samples, ≥10 samples per short test, stddev intervals** (#147): every result of both harnesses keeps `samples_ns`; `weft-bench verify` flags a short result under 10 samples; `weft-xbench` defaults to 10 reps
  - [x] **DONE (2026-11-02) — result hashes** (#147): `correctness.result_sha256` over each workload's canonical output; the storage error bound gates publishing
    - [x] **DONE (2026-11-02) — the same digest in `weft-xbench`** (`answer_sha256`, report v6): per arm, values to twelve significant digits, so separate runs of one arm can be shown to agree; arms are still compared with each other by the max-diff gate
  - [x] **DONE (2026-11-02) — several runs per report** (`weft-bench combine`, #147): pooled samples, intervals drawn runs-first, `between_run` per result; two runs of one build 41.9% apart no longer read as a regression (DECISIONS 2026-10-29, EVI-15)
    - [x] **DONE (2026-11-02) — the same in `weft-xbench`** (`weft-xbench combine`): run-first intervals, and a rendered win or loss only where every run's median agrees; a one-run report is flagged as one
  - [x] **DONE (2026-11-02) — saturation sweeps and a seeded query mix** (`weft-bench sweep KNOB=v1,… -- ARGS`, `--query-mix OPS`): any run flag or the worker pool swept into curves with their knees (2M-row UCI ingest: 109k → 4.57M rows/s from 1k- to 256k-row batches, no knee yet); the soak lanes replayed against one store from a published seed, every read exact
  - [x] **DONE (2026-11-02) — a run manifest for `weft-xbench`**: input file and binary SHA-256, and with `--container` each engine container's image digest, command and environment (secrets redacted)
  - [x] **DONE (2026-11-02) — post-compaction reads** (`--compressed-query-compact ROWS`): the store's own squash (`squash_aspect_to_target_rows`), then the same seeded windows, gated on identical answers
  - [x] **DONE (2026-11-02) — failure tests** (`weft-bench --failure kill-ingest|kill-compaction|corrupt-segment`): a child process killed with SIGKILL mid-ingest or mid-compaction, or a flipped byte in a closed segment; the reopened store must hold every acknowledged row exactly and quarantine, never serve, a damaged segment (open-time recovery caught all 143 byte flips of a probe frame). Network retry is SOA-8's failure deck (#502); the CI crash matrix stays D-S21/SOA-9's
    - [x] **DONE (2026-11-03) — compaction cost** (#507): squashing 10M rows from 9,769 to 153 segments took 694 s (361 s on a quiet machine), nearly all of it one keyed index delete per merged segment under Turso's MVCC; one guarded delete per group makes it 31–43 s. Small windows read ~36× slower after it mostly because the index's deleted versions were never checkpointed; with a checkpoint after the squash an hour's p50 is 0.49 ms (0.22 ms before the squash, 2.2 ms without the checkpoint). The rest was decoding a 65k-row segment's value column from its start; merges now write 4,096-row pages, so an hour reads 0.18 → 0.25 ms across the squash for 1.341 → 1.357 B/pt (still below the 1.414 of the seals it replaces)
- [x] **DONE (2026-11-02) — Fair interpolation comparisons (Phase 1.2)** (#148): every `weft-xbench` engine answers in three classes where it can — its native fill (A: TimescaleDB, QuestDB, InfluxDB 3, IoTDB, ClickHouse), portable SQL (B) and client-side (C, all six) — with native fills that compute another answer gated on shape and reported against the line, never raced; each ASOF strategy run and published (#393). A `tiny` run passed every arm on all five servers
  - [x] **DONE (2026-11-02) — the pooled cross-engine evidence run** (#508): two full runs (`small,medium`, UCI, TSBS, all five servers) pooled with `weft-xbench combine`, in `bench/reports/phase1-pooled/`. 319 of 320 gates pass; the one failure is TimescaleDB's `double` downsample, whose answer differs between runs in the last bits, so `combine` refuses to pool it. 105 of 112 verdicts hold in both runs, and none flips between a win and a loss: 17 wins, all against portable SQL (five at `medium`, TSBS and UCI interpolation), 85 losses (every load, the stored downsample, and every comparison with client-side interpolation) and 3 within noise
- [x] **DONE (2026-11-02) — Report surfaces beyond JSON/HTML/Parquet** (#149): Markdown, CSV and Prometheus text (`--markdown-report`, `--csv-report`, `--prom`, `weft-bench export`), and a Grafana dashboard over the Prometheus text (`bench/grafana/`)
- [x] **DONE (2026-11-02) — Anti-Goodhart (Phase 1.3)** (#150): `scripts/bench/check-separation.py` in CI (no engine crate depends on or detects a benchmark crate); HONESTY.md maps every rule to what enforces it

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
  - [ ] **Prometheus remote write/read**: follows **B-tags**. A remote-write series is a metric plus a label set, and without per-measurement tags each label set would become its own aspect (DECISIONS 2026-11-04).
  - [x] **R/Arrow workflows** (2026-11-04): `clients/r` (package `weftdb`) reads stored windows, value bands and server-side downsamples as Arrow tables, pages the exact JSON rows, and writes data frames or Arrow tables as one Parquet batch with `Idempotency-Key`. Refusals are `weft_<code>` conditions. `tests/testthat/test-live.R` is the contract against a running server. It found that Parquet ingest refused Snappy, the default codec of R's arrow, pyarrow, pandas and Spark; the workspace's `parquet` now reads it.

### Phase 3 — Instrument everything · *Very high*

The metrics side is done: a Prometheus `/metrics` surface, latency histograms over the compute and seal paths, a `/debug/profile/current` endpoint, and a harness-level timing breakdown covering dataset generation, adapter and whole run. Affected-row accounting from Turso's `Statement::n_change()` is recorded for the aspect-metadata write and the seal's index write, both on the seal path.

Tracing has a `RUST_LOG`-driven foundation, a per-request root span, and per-stage child spans that reach the storage read path and the Arrow/Parquet decode on ingest. OTLP trace export is in, and delivery to a real collector has been verified.

- [x] Remaining per-stage spans
  - *Closed 2026-10-27: every stored read closes a `.serialize` span tagged with its `format` (CSV, Arrow, Parquet, JSON, point and at-multi). The Arrow and Parquet encode now runs as its own stage in the handler, so it is timed apart from the read. The point and at-multi `.read` spans carry `format` too. `every_stored_read_format_closes_its_serialize_span` in `weft-server/src/storage.rs` asserts each one. The downsample handlers' `downsample.range` span is not a `.read` span and is outside this item's list.*
- [x] **`Statement::n_change()` accounting on the seal path's index write**
  - *Closed 2026-10-27: `publish_seal` wraps its `InsertNew` commit in a `control_plane.index.insert` span whose `rows_changed` is that op's affected-row count, taken from the `TxnApplied` the commit already returns. `weftdb/tests/seal_index_span.rs` asserts the span.*
- [x] `/bench/runs/:id` endpoint
  - *Deferred 2026-10-27, pending a design decision: nothing named a run, and serving provenance before SEC-6/SEC-8 would have exposed it.*
  - *Closed 2026-11-03: a run id is the SHA-256 of the manifest file's bytes, content-addressed rather than assigned, so the manifest schema is unchanged and `weft-bench` prints the id beside the manifest path. With `WEFT_BENCH_RUNS_DIR` set, `GET /bench/runs` lists the manifests under it and `GET /bench/runs/{id}` serves one byte for byte, immutable, with the id as its `ETag`. An unknown id is `404 run_not_found`. Both routes sit in the `metrics` scope (maintainer and admin, behind SEC-8's exposure matrix) at the experimental tier, outside the 1.0 freeze. The tests in `weft-server/src/bench_runs.rs` cover the walk (depth cap, symlinks not followed), the bytes and `ETag`, 400/404/503, and the role check.*

### Phase 4 — Hot path: physical types & Storage v2 · *High*

The physical-type groundwork is done: integer-epoch `TimeUnit` with lossless delta and delta-of-delta transforms, a zig-zag/LEB128 varint estimate, RLE and a `best_estimated_bytes` selector, fixed-width bit-packing, and Arrow-compatible arrays (4.5). Monotonic order is detected and enforced, and the read planner picks the cheapest way to answer a point or range read.

Out-of-order data is fully handled (4.6): in-place per-segment sort, cross-segment merge, the split-not-rewrite optimisation, threshold-triggered background reconciliation, and a hot/cold split in the background sweep. Reads skip work by pruning on time and value at segment level, and on pages within a segment. Quality pruning of all-null segments and pages is wired into the point-read and downsample paths (see 4.3): the index prune drops a fully null segment before its file is opened, and a frame's persisted stats drop fully null pages. A range read still reads every time-overlapping segment, because a null row is part of its answer; a downsample is not, because a row that carries no value moves no bucket.

- [x] **4.1 Physical numeric encodings**
  - [x] Explicit tz + leap-second policy
    - *Landed 2026-10-28 (`w2/tz-policy`). Stated on `TimeUnit` (`weft-physical-type/src/timestamp.rs`) and pinned by three tests: epochs are UTC ticks since the Unix epoch under the POSIX rule, no zone is stored, no leap second is modelled (a stored day is exactly `86_400 × nanos_per_unit()`, so a real leap second is a counted-twice second, not a discontinuity — no table, no smearing, no TAI-UTC answer). The unit arithmetic the policy implies is now `TimeUnit::{nanos_per_unit, to_nanos, from_nanos}` — `to_nanos` fails rather than wrapping past the nanosecond range, `from_nanos` truncates toward minus infinity — and `scaled_rows` in `weftdb/src/types/segment_store.rs` uses it instead of its own per-unit match. The chrono API boundary is unchanged: an instant enters and leaves as a UTC `DateTime` and only unit arithmetic is applied.*
- [x] **4.3 Columnar segment store**
  - [x] Quality pruning wired into the read path (all-null segment/page skipping)
    - *Landed 2026-10-27 (`w2/qprune-read-path`). Three levels, each at the layer that owns the decision: `SegmentIndexStore::prune_present_descriptors` (`weftdb/src/types/segment_index.rs`) is the SQL mirror of `SegmentIndex::prune_present_by_time` (`weft-physical-type/src/catalog.rs:395`) and is what `SegmentStore::read_point`/`read_points` now prune with, so a segment indexed as all-null never has its file opened; `read_segment_point(s)`/`read_paged_segment_point(s)` (`weft-physical-type/src/weftseg.rs`) drop a fully null frame and fully null pages from the persisted stats, reached through `read_frame_point(s)`.*
    - *Deliberately not pruned: `read_time_range` returns null rows, so dropping a segment there would delete timestamps the caller asked for — a range read over a window holding a corrupt all-null frame still reports the damage, which is what pins this boundary in `point_reads_prune_an_all_null_segment_without_opening_it`. `read_segment_instant` — the last-writer-wins reader behind `Segment::query`/`query_as_of` — is not pruned either, because skipping a segment can change which segment wins. `prune_by_time` stays the ingest primitive for the same reason (`weft-reduce/src/windowed.rs:55`, `bounded.rs:57`). The page-level `prune_present_pages_by_time` (`page.rs:364`) stays a resident-index helper: the store's range reads stream values straight out of blocks, where an all-null page costs one u64 header compare and appending its nulls is cheaper than filtering them back out.*
    - *Extended 2026-10-28 (same branch): `downsample_range` now prunes with `prune_present_descriptors` too. A null row is part of a range read's answer but not of a bucket's: it enters no sum, count, min, max, first or last, and a partial holding nothing is dropped when buckets merge, so a window covered only by nulls yields no buckets rather than a run of empty ones. `an_all_null_segment_changes_no_bucket_of_a_downsample` pins that against the single-pass `reduce` over the live rows, at two resolutions and seven aggregations, including a window the null segment covers alone.*
  - [ ] Tag pruning *(blocked on B-tags — per-measurement tags/labels don't exist yet)*
- [ ] **4.6 Correctness semantics**
  - [ ] Out-of-order **reconciliation — configurable upsert keys + skip-identical**:

### Phase 5 — GPU interpolation flagship · *High (parallel w/ Phase 4)*

Interpolation and extrapolation run on the GPU, supported by a size-tiered LRU buffer pool, persistent staging buffers and an async-handle scaffold. The storage half of the path is done, and the 2026-10-09 measurement showing where end-to-end time goes, and what the `f64` egress costs, is complete.

- [ ] 5.1 Command batching
- [ ] 5.2 True async GPU handles (non-blocking; CPU parse/read overlaps GPU)
- [ ] 5.3 CPU/GPU overlap (chunked read → decode → upload → kernel → stream output)
- [x] 5.4 Hardware auto-tuning (calibrate CPU/GPU throughput, transfer cost, break-even sizes; auto-select)
  - *Audit 2026-10-27: checked — `weft-server/src/gpu.rs:82` `calibrate()` runs `splimes::calibrate()` at startup (`gpu.rs:95`), timing GPU, rayon pool and single core and installing the measured break-even thresholds that `Backend::Auto` then uses to auto-select (`gpu.rs:1-20`, `settings.rs:109` `WEFT_GPU_CALIBRATE`; tests at `gpu.rs:138-193`).*
- [ ] 5.5 GPU economics (cloud cost, CPU-only fallback, utilization/contention/cold-start, NVIDIA/AMD/Intel/Apple portability)
- [ ] 5.6 wgpu/WGSL portability & determinism (conformance matrix, numerical drift, published tolerances)
- [ ] 5.7 Multi-GPU *(deferred until single-GPU wins are proven)*
- [ ] Memory-mapped GPU I/O *(only after transfer bottlenecks are measured)*
- [ ] End-to-end GPU interpolation benchmark (storage read → … → API serialization, with p95)
  - [x] **FOUND (2026-10-11, runtime) — `POST /api/v1/downsample` rejected the documented multi-word tokens. Closed 2026-10-28 (FRE-5, the spelling half):** the JSON body reads `Aggregation::from_token`, so `twa_linear`, `sketch_p99` and the aliases parse as they do in a query string, and the bare lowercase names still parse. Tokens are exact-case as of 2026-10-29 (FRE-5's first clause, which breaks v0.1.0's case folding; see the CHANGELOG): `Min` and `MEDIAN` are a `400`. The rest of FRE-5, "no silent narrowing", closed 2026-11-01 (FRE-5): a computed value with no finite `f64` representation now fails the compute request with `not_finite` instead of returning a substitute `0.0`.
  - [ ] **FOUND (2026-10-09) — the compute endpoints widen JSON `f64` values with `BigDecimal::from_f64`** (`weft-server/src/downsample.rs:138` …
  - [ ] **NEXT — the compute endpoints pay the same tax:**
  - [x] **Reconcile with EVI-10A** (2026-10-19: the API now has an input cap, `interpolate_range::MAX_KNOTS`, a `pub` const the review may keep, turn into an option or drop)
  - [ ] **NEXT (post-1.0 per decision 18) — the HTTP endpoint:**
- [x] **DONE (2026-10-09) — GPU memory-stability + pool-eviction benchmark** (`weft-bench --gpu-memory`, `gpu_pool` in schema v22)
  - [ ] **NEXT — device memory, not just pool bytes:**

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
  - [ ] **S20** Change log replaces the per-timestamp unbatched queue for seal-backed aspects *(~7 h; after S19)*
  - [x] **S21** Crash-matrix CI, subprocess SIGKILL soak, durability benchmark and ROADMAP re-baseline *(~7 h; after S14, S16)*
- [ ] **7.3 Corruption detection**
  - [ ] Restoring *into a chosen new root* over HTTP
  - [ ] Whole-store backup manifest (fold the `.weftseg` frames in beside the control plane)
  - [ ] Document RPO/RTO against the measured snapshot cadence
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
- [ ] **7.4 Whole-store backup manifest**
- [ ] **7.4 Documented RPO/RTO** against the measured snapshot cadence
- [ ] **7.5 Compaction** with query consistency, cancellation, and resource limits
- [ ] **7.6 Quotas/limits**
- [ ] **Phase 7 acceptance run**

### Hot path & data model *(must land before the freeze)*

`splimes` 1.0 is released and WeftDB depends on it, and the control-plane engine decision is made.

- [ ] **Measurement bulk ingest routed through the `.weftseg` seal**
- [ ] **B-tags**

### Security & packaging *(Phase 8 minimum)*

- [ ] TLS on `weft-server`
- [ ] API-key/token auth + basic RBAC + audit log
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
  - [x] **FRE-1** Release truth, per-crate versions, single edition, publish split, package hygiene
  - [x] **FRE-3** Golden frames, golden sidecars and persisted-encoding goldens (gated codecs included)
  - [x] **TAG-3** Open `ValueKind` in weftdb, provenance scope
  - [x] **EVI-1** Claims hygiene (2026-10-25: every command, flag, endpoint and link in the README
    checked against the code; the one over-claim — that any figure is reproducible from a clone —
    is reworded, and per-figure sourcing becomes EVI-2's job)
  - [x] **EVI-3** Run manifest (schema v23: the 2026-10-09 routine took v20–v22, so EVI-3 takes the next free number per the plan's rule)
  - [x] **EVI-9** Block missingness and scale presets
  - [x] `weft-bench --sparse`: a seeded (series × block) grid of nullable cells, the block-aligned mean scan over it, and the observed missingness recorded in the report beside the grid that produced it
  - [x] `--scale tiny|small|medium|large` for the sized workloads (`--sparse`, `--gap-fill`, `--downsample`), filling only the size knobs left at their default, and naming the preset in the profile so two grid sizes never compare as one run
  - [x] **SOA-1** Soak harness skeleton
- [x] **W2 · Error contract, frame format, portable builds**
  - [x] Legacy lane
    - [x] **FRE-14c** operator route: `weft-server import-rows-mode` reads one 0.1.x subject into a Storage v2 store, prints what stays behind and why, and records the run so a second read refuses
    - [x] **FRE-14c** streaming rows-mode read: a whole aspect must not have to fit in memory
    - [x] **FRE-14c** migrating `metadata.db`'s non-row state
  - [x] **PAC-2** Portable Linux binaries on a glibc 2.28 baseline
  - [x] **EVI-5** Nightly benchmark-regression workflow and `weft-bench compare`
  - [x] **EVI-2** Claims registry and markdown claim/link lint
  - [x] `bench/claims.json` vouches for each figure the README quotes: the bench or the local corpus behind it, the date, and whether a clone can re-run it
  - [x] `tools/check_claims.py` in CI pairs every entry with its `<!-- claim:id -->` marker, checks the figure text still sits beside the marker and the source path still exists, fails on a `*Source:*` line no entry claims, and resolves every relative link in the README against the tree
  - [x] → **EVI-4** Artifact pipeline
  - [x] **EVI-6** Ingest workload over SegmentStore and the UCI corpus loader
  - [x] **EVI-8A** Benchmark adapter v2 I
  - [x] **SOA-2** Soak preflight, device resolution, run metadata and secret redaction
  - [x] **SOA-3** Open-loop scheduler, HDR interval logs, harness self-monitoring and null server
  - [x] **SOA-4** Deterministic workload model, lanes, format-exact value grids and body encoders
- [x] **W3 · Gate, serve path, input domain**
  - [x] Store lane
  - [x] → **EVI-10A** Library stored-range interpolation with input caps (a child module, one `mod` line)
  - [x] → **D-S11** Rollup into segment_index.db, folded in the seal and swap transactions
  - [x] **SEC-3** Sealed route registry (`RouteSpec`), frozen scope table and fail-closed gate
  - [x] **SEC-4** One HTTP/1.1 serve path with pre-auth listener limits (absorbs ROB-23's caps)
  - [x] **ROB-6** Bounded decimals
  - [x] **EVI-8B** Benchmark adapter v2 II
  - [x] **SOA-5** Soak ingest client
  - [x] **SOA-12** Linux system samplers, contamination flags and the strict-mode flush check
- [x] **W4 · Auth, config, ILP fix**
  - [x] Store lane
  - [x] → **D-S13** Idempotent atomic ingest entry point (library) (+ `groups`, ledger `series_ids`, typed errors)
  - [x] **SEC-5** TLS listener (rustls with ring) on the shared serve path
  - [x] **SEC-6** API keys, Bearer authentication, keys file and keygen
  - [x] **ROB-3** Panic containment, daemon supervision, Daemons registry, `weftdb::exec`
  - [x] **PAC-5b** Strict server configuration and store filesystem check (the bind guard moves to SEC-8)
  - [x] **TAG-0** Fix ILP series merging
  - [x] L-FMT: **ROB-8** Frame decode hardening
  - [x] **EVI-11** Stored WeftDB benchmark adapter
  - [x] **SOA-6** Soak oracle core
- [x] **W5 · Ingest wiring, whole-store backup, legacy seal**
  - [x] Store lane
  - [x] → **D-S15** Partial sidecar v4
  - [x] → **D-S19** Seal-backed legacy aspects
  - [x] L-INGEST
  - [x] **ROB-4** Enforced no-panic lint (including asserts) and the weft-arrow Result API
  - [x] **ROB-10** Admission I
  - [x] **SEC-8** Startup ordering, exposure matrix, auth-off guard, metrics auth
  - [x] → **SEC-10** Exposure and secrets hygiene
  - [x] **FRE-5** Compute vocabulary, exact-case tokens and no silent narrowing
  - [x] **FRE-7b** Immutable declarations, scale rules and schema read-back
  - [x] **EVI-13** DuckDB adapter I
  - [x] **EVI-15R** Report renderer with honesty-section generation
  - [x] **EVI-16** Honesty pages I
  - [x] **SOA-7** Window digests and the oracle self-test
  - [x] **SOA-8** Failure deck, phases, recovery windows, SIGTERM verdict and the maintenance driver
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
- [ ] **W7 · Post-split lanes open; security and packaging close**
  - [ ] open lane
  - [x] → **FRE-12b** Scope pin, `GET /api/v1/version`, incompatible-store mapping and exit code
    - The pin refuses a start whose `WEFT_STORE_DATABASE`/`WEFT_STORE_SUBJECT` differ from the
      scope the store root's own `STORE_FORMAT` records (exit 65, the class the exit table has
      always named for a scope mismatch; `WEFT_STORE_SCOPE_PIN=off` serves the root anyway).
      `GET /api/v1/version` answers build, layout, scope-pin and exit-code state from process
      state alone, so it answers while a store is being refused. DECISIONS 2026-11-03
  - [ ] seal lane
  - [ ] read lane
  - [ ] → **ROB-13** Bounded stored downsample and point lookups
  - [ ] **ROB-7** Ingest decode bounds
  - [ ] **ROB-16** GPU configuration, compute limits, interpolation provenance and metrics
  - [ ] **ROB-23r** Probe-only admin listener
  - [x] **PAC-13** Offline restore CLI over D-S16 (`weft-server restore`)
  - [x] **ROB-5** Legacy queue-consumer panics
  - [ ] **SEC-13** Security acceptance
  - [x] **PAC-9** Docker image and container smoke test (with a mounted keys file)
  - [ ] **PAC-11** systemd unit, user and env files, and host-install CI
  - [x] **EVI-15** First cross-engine report
  - [x] Write or approve the cross-engine report narrative *(owner)*
  - [x] **EVI-19** Documented RPO/RTO
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

The interpolation item (B-interp, Phase 5) is done.

- [ ] **B-tags** *(Phase 2/4 · absent)*
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
- [ ] **Backup 7.4 residue (d) — whole-store backup manifest**
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
- [x] **Historical note — the LNK1102 research that produced the `[profile.test] debug = 1` fix:**
  - *Audit 2026-10-27: checked — this is a historical note and its fix shipped: `[profile.test] debug = 1` is at Cargo.toml:128.*
- [ ] **Guard the `#[serial]`-above-`#[test]` ordering (research 2026-07-20):**
- [ ] **NEXT — UDDSketch (uniform guarantee under collapse):**
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
- [ ] Publish a methodology document **before** any performance claim
