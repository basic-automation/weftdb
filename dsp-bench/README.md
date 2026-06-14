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
  byte-for-byte reproducible). The noise-free underlying signal is selectable via
  `SignalShape` (`MultiSine`, `Sawtooth`, `Step`, `DampedSine`) — each a
  deterministic, `[10, 90]`-bounded analytic curve that doubles as the accuracy
  ground truth — so reconstruction quality can be probed across smooth vs.
  sharp-discontinuity signals, not just one shape.
- **Vendor-neutral adapter trait** (`src/adapter.rs`) — every benchmarked system
  is driven through `SystemAdapter`; concrete adapters stay decoupled from the
  harness core, mirroring the roadmap's connector hard-constraint.
- **DSP reference adapter** (`src/dsp_adapter.rs`) — drives DSP's native
  interpolation engine (`splimes::auto_interpolate`).
- **Portable linear baseline adapter** (`src/baseline_adapter.rs`,
  `BaselineLinearAdapter`) — the fair-protocol *class (C)* client-side baseline:
  a dependency-free, precision-aware (`BigDecimal`-throughout, no silent `f64`
  downcast) piecewise-linear reconstruction implemented directly in the harness.
  It is DSP-Bench's first *second* system, so a report can carry a real
  two-system comparison rather than a lone number. It is always linear by
  definition (it ignores the requested spline) and named `baseline-linear`, so a
  comparison against DSP's chosen method is an honest quality/speed reference,
  not a disguised apples-to-apples spline race.
- **Portable forward-fill (LOCF) baseline adapter** (`src/forward_fill_adapter.rs`,
  `ForwardFillAdapter`) — the in-process mirror of the gap-fill real time-series
  engines actually ship: InfluxDB's `FILL(previous)`, QuestDB's `FILL(prev)`,
  TimescaleDB's `locf()`. It reconstructs each grid point by holding the latest
  prior sample (a piecewise-constant step), copying that sample's exact
  `BigDecimal` verbatim — never arithmetic, so no float drift. Before the first
  sample it holds the first value backward (the standard finite-valued choice when
  no back-fill exists). Always forward-fill by definition (ignores the requested
  spline) and named `baseline-forward-fill`, so a comparison gives DSP's
  interpolation against the *fair-protocol class (B)* native-gap-fill behaviour,
  not only the linear baseline.
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
- **Reconstruction-accuracy metrics** (`src/accuracy.rs`, `AccuracyMetrics`) —
  the quality axis beside the speed axis (fair-protocol Phase 1.2 / Phase 6.4).
  The synthetic generator samples a known analytic signal, so a reconstruction's
  output grid can be scored against the *true* shape it was meant to recover:
  **RMSE / MAE / max-error / bias**, via `from_aligned`, with
  `synthetic_ground_truth` evaluating truth at the same grid the adapter
  produces. This lets the three reconstruction methods (DSP spline, linear,
  forward-fill) be compared on accuracy, not only speed. A line-protocol source
  has no ground truth, so it carries no accuracy. (These are error *statistics*
  over an analytic `f64` truth — a reporting quantity, not a stored or hot-path
  measurement value, so the no-silent-downcast rule is unaffected.)
- **Runner** (`run_profile` in `src/lib.rs`) — runs a profile against an adapter
  for N timed reps and produces a `BenchResult`, recording the end-to-end timing
  spans (dataset generation vs. measured operation vs. whole run) and, for a
  synthetic profile, the reconstruction accuracy of the final rep (`measure_accuracy`
  is also exposed standalone).
- **JSON report runner** (`src/report.rs`) — wraps one or more `BenchResult`s in
  a `BenchReport` envelope with lightweight run metadata (`dsp-bench` version,
  OS, CPU arch, generation timestamp) and persists it as pretty-printed JSON to a
  `reports/json/` artifact, satisfying the "keep raw results" reproducibility
  rule. A report is publishable only when every result it holds is publishable.
- **InfluxDB Line Protocol ingest** (`src/line_protocol.rs`) — a dependency-free
  ILP *format* parser (`parse` → `LineRecord`s; `parse_points` → sorted
  `splimes::Point`s for a chosen numeric field). Handles tags, typed fields
  (float/int/unsigned/bool/quoted-string), `\,`/`\ `/`\=` and in-string `\"`
  escaping, comments, and caller-declared timestamp precision
  (`TimestampPrecision`). This is the cheapest path to **TSBS compatibility**; it
  is a format parser only — a concrete InfluxDB *connector* (network client)
  stays outside the core per the connector hard-constraint. The parser is wired
  end-to-end through a workload via
  `InterpolationProfile::from_line_protocol` (`DatasetSource::LineProtocol`), so a
  real `.lp`/TSBS payload drives the same interpolation harness, correctness
  gate, and reporting as the synthetic flagship profile.
- **Command-line runner** (`src/main.rs`, binary `dsp-bench`) — two input modes:
  read a `.lp` / TSBS file and project a chosen numeric field, **or** `--synthetic`
  to drive the seeded generator (knobs: `--seed`, `--points`, `--missingness`,
  `--jitter`, `--noise`, `--shape`). Only the synthetic mode has a known ground
  truth, so only it reports accuracy. Writes a `BenchReport` JSON artifact to
  `reports/json/`. Pure hand-rolled arg parsing (no `clap`); exits non-zero when
  the correctness gate fails so a scripted caller can gate on it.

## Not yet (tracked in `ROADMAP.md`)

External-engine competitor adapters (DuckDB, ClickHouse, InfluxDB 3, QuestDB,
TimescaleDB) — the portable linear and forward-fill baselines above are the first
*non-DSP* systems, but they run in-process rather than against a real database. The
Phase-2 server-side ILP *ingest endpoint* (the file/CLI ingest path exists; an
`axum` HTTP endpoint is next), additional workloads (range fetch, downsample,
compression, …), dataset corpora, the richer report formats (Parquet/HTML) and
full hardware capture (CPU model, RAM, GPU, drivers) in run metadata, and the
methodology document.

## Run

Smoke-test the harness:

```sh
cargo test -p dsp-bench
```

The smoke test generates the `interpolation-heavy-irregular` dataset, runs it
through the DSP adapter, and asserts the produced grid is correctly sized,
finite, and that the result schema round-trips through JSON.

Run a benchmark over a line-protocol / TSBS file and emit a JSON report:

```sh
cargo run -p dsp-bench -- \
    --input data.lp --field usage \
    --precision s --spline cubic --resolution minutes --reps 10
```

The report lands at `reports/json/<profile>__dsp.json` (the profile name defaults
to the input file stem). `--help` lists every flag; the precision, spline, and
resolution accept short forms (`s`, `cubic`, `m`).

Add `--compare` (`-c`) to run the portable baseline suite (linear + forward-fill)
alongside DSP and emit a single comparison report holding all three results:

```sh
cargo run -p dsp-bench -- \
    --input data.lp --field usage --spline cubic --resolution minutes --compare
```

The artifact name tags every adapter, e.g.
`reports/json/<profile>__dsp+baseline-linear+baseline-forward-fill.json`, and the
process still exits non-zero if *any* result's correctness gate fails.

### Quality comparison (synthetic mode)

`--synthetic` runs the seeded generator instead of a file. Because its underlying
signal is known analytically, the report adds **accuracy** (RMSE / MAE /
max-error / bias) for every system — the only mode that can:

```sh
cargo run -p dsp-bench -- \
    --synthetic --points 300 --noise 0 --shape sawtooth \
    --spline cubic --resolution minutes --reps 5 --compare
```

`--noise 0` puts every sample exactly on the ground truth, isolating each
method's own reconstruction error; raise it to probe robustness. `--shape`
(`multisine` | `sawtooth` | `step` | `dampedsine`) selects the analytic
ground-truth curve, which is recorded in the artifact (schema v5) so the dataset
regenerates exactly from seed + knobs + shape. The run prints a `signal shape`
line, an `accuracy : rmse=.. mae=.. max=.. bias=..` line per adapter, and the
metrics land in the JSON artifact.

Honesty note: the winner depends on the shape, and DSP-Bench surfaces that rather
than hiding it. On the smooth high-frequency `multisine` (and on `step`) DSP's
cubic spline leads on RMSE, but on the `sawtooth` the portable linear baseline
*out-accuracies* the cubic — the cubic overshoots the sharp discontinuities — so
the "most accurate" line names linear, not DSP.
