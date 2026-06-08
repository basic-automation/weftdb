# dsp-bench

DSP-Bench is DSP's reproducible, correctness-gated benchmark harness — the first
track of the roadmap's benchmark-led commercial thesis. It is both an internal
engineering suite and a public/customer-runnable diagnostic: every number it
emits ships with the dataset seed, the workload profile, and a correctness
verdict, so results are reproducible and trustworthy rather than synthetic wins.

> Policy: *DSP benchmarks guide real engineering decisions, not synthetic wins.*
> A latency number is only publishable when its correctness check passes.

## Status — scaffold

This is the initial scaffold. What exists today:

- **Workload profile + dataset generator** (`src/profile.rs`) — the flagship
  `interpolation-heavy-irregular` profile generates a seeded, irregularly-spaced,
  gap-containing series over a fixed time span (`ChaCha8Rng`, published seed →
  byte-for-byte reproducible).
- **Vendor-neutral adapter trait** (`src/adapter.rs`) — every benchmarked system
  is driven through `SystemAdapter`; concrete adapters stay decoupled from the
  harness core, mirroring the roadmap's connector hard-constraint.
- **DSP reference adapter** (`src/dsp_adapter.rs`) — drives DSP's native
  interpolation engine (`splimes::auto_interpolate`).
- **Result schema** (`src/schema.rs`) — serializable `BenchResult` capturing the
  latency distribution, dataset metadata, correctness verdict, and an
  **end-to-end timing breakdown** (`TimingBreakdown`: one-time dataset-generation
  cost, the summed measured adapter calls, and the whole-run span) that makes it
  auditable that the latency distribution times only the adapter call, never
  dataset setup.
- **Latency statistics** (`src/stats.rs`) — p50/p95/p99 + min/max/mean/stddev
  (nearest-rank percentiles), plus **seeded bootstrap confidence intervals**
  (`LatencyStats::bootstrap_cis`) for the mean and the p50/p95/p99 percentiles.
  Resampling is driven by a published RNG seed, so every interval is exactly
  reproducible — per the fair-protocol requirements.
- **Runner** (`run_profile` in `src/lib.rs`) — runs a profile against an adapter
  for N timed reps and produces a `BenchResult`, recording the end-to-end timing
  spans (dataset generation vs. measured operation vs. whole run).
- **JSON report runner** (`src/report.rs`) — wraps one or more `BenchResult`s in
  a `BenchReport` envelope with lightweight run metadata (`dsp-bench` version,
  OS, CPU arch, generation timestamp) and persists it as pretty-printed JSON to a
  `reports/json/` artifact, satisfying the "keep raw results" reproducibility
  rule. A report is publishable only when every result it holds is publishable.

## Not yet (tracked in `ROADMAP.md`)

Competitor adapters (DuckDB, ClickHouse, InfluxDB 3, QuestDB, TimescaleDB),
InfluxDB Line Protocol ingest, additional workloads (range fetch, downsample,
compression, …), dataset corpora, the richer report formats (Parquet/HTML) and
full hardware capture (CPU model, RAM, GPU, drivers) in run metadata, and the
methodology document.

## Run

```sh
cargo test -p dsp-bench
```

The smoke test generates the `interpolation-heavy-irregular` dataset, runs it
through the DSP adapter, and asserts the produced grid is correctly sized,
finite, and that the result schema round-trips through JSON.
