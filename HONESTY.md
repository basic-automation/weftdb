# Honesty pages

What WeftDB is not for, how its numbers are produced and checked, and what "exact" does and does
not mean. Each claim below links to the code, benchmark or measurement it rests on. If a link and a
sentence disagree, the link wins and this page is wrong; please open an issue.

These are the first four of the six honesty pages in the commercial thesis (evidence track
EVI-16). *When WeftDB beats general TSDBs* and *GPU tuning and economics* follow in EVI-17, once the
cross-engine report has a run on the reference host. The narrative is the project owner's to
approve before it is published anywhere else.

- [When WeftDB is not the right tool](#when-weftdb-is-not-the-right-tool)
- [Benchmark methodology](#benchmark-methodology)
- [Interpolation-accuracy methodology](#interpolation-accuracy-methodology)
- [Precision and physical value types](#precision-and-physical-value-types)

---

## When WeftDB is not the right tool

Use something else if any of these describe you. Each is either a measured loss or a missing
feature, not a modesty.

**You need a hardened, internet-facing production database.** As of this page there is no TLS,
no authentication, no RBAC and no audit log. They are 1.0 release criteria, not shipped features
(see the README's *Project status* and `ROADMAP.md`). Pre-1.0, WeftDB should run only on an
isolated network.

**Your workload is bulk loading.** On the first cross-engine run (2026-10-29, `weft-xbench`, see
`DECISIONS.md` EVI-13), DuckDB loaded the same series **10–29× faster** at every scale: 174 ms
against 5.1 s for about 600k rows. That comparison is not like-for-like. WeftDB sealed about 590
durable 1,024-row segments, while DuckDB did one bulk append and one checkpoint. But it is the
layout WeftDB's read path was measured on, so it is reported as a loss.

**Your series are large and you need exact-decimal output.** In the same run, at `medium` scale
(about 600k input points into an 86,401-point grid), DuckDB's hand-written linear interpolation in
SQL was **about 3–4× faster** than WeftDB returning `BigDecimal` points: 45.8 ms against 139.7 ms
in memory and 174.6 ms from storage. WeftDB's `f64` output path was level with DuckDB there and
ahead at smaller sizes. If you need exact decimals on very large grids, budget for that cost.

**You want generic OLAP over wide tables.** WeftDB does not compete with ClickHouse or DuckDB on
broad analytical scans, and has no SQL.

**Your data is regular and complete.** If nothing is missing and nothing needs resampling,
WeftDB's core advantage does not apply, and a conventional TSDB is the simpler choice.

**You expect splines to beat straight lines on market data.** On real BTC/USD closes, a cubic gap
fill had MAE $55.34 against linear's $49.27 (hourly buckets), and $8.37 against $6.79 at
one-minute buckets, at about 14% more time. Prices move close to a random walk. The spline wins
only on smooth signals (README, *Where it doesn't pay off*).

**You expect a GPU to make every query faster.** Reconstructing about 1M points from 100k stored
knots took 14.1–16.3 ms on CPU threads (rayon) and 17.0–29.8 ms on an RTX 4070 Ti SUPER
(`weftdb/benches/interpolate_range.rs`). The GPU is not the fastest backend on that path.

**You need tags, labels or high-cardinality dimensional slicing.** Per-measurement tags are not
shipped. WeftDB's data model is not Prometheus-shaped.

**You need last-observation-carried-forward over stored data.** LOCF (`prev`) fill exists for
bucketed reductions, but not for stored-range interpolation in 1.0.

**You need managed hosting or a support contract today.** There is neither. Hosting is a planned
paid offer and support is priced, but nothing is for sale until Basic Automation is incorporated
(`WILLINGNESS-TO-PAY.md`).

---

## Benchmark methodology

Every figure WeftDB publishes is produced by one of two harnesses and must pass that harness's
correctness gate before its timing counts.

**The harnesses.**

- `weft-bench` measures WeftDB's own paths against portable in-process baselines: linear and
  forward-fill reimplementations. It covers interpolation, ingest, point and range reads,
  compression, downsampling, gap fill and block-sparse scans.
- `weft-xbench` measures WeftDB against another engine (DuckDB today) on load and linear
  interpolation. It is a separate workspace, because DuckDB builds from C++, with a lockfile
  seeded from the workspace's and checked for drift in CI.

**Correctness before speed.** A run's latency is publishable only if its correctness gate
passes. Each workload defines its gate: the output grid has the expected size and every value is
finite; a read equals a full decode; a compressed column round-trips exactly; buckets match the
serial reduction. In `weft-xbench` every arm must also match WeftDB's in-memory reconstruction to
a relative 1e-9. A failing result is still reported, first, with its timing marked invalid. It is
never dropped.

**Reproducibility.**

- Synthetic data comes from a seeded `ChaCha8Rng`, and the seed is in every report.
- Each run writes a manifest (EVI-3) with the toolchain, build profile, hardware, disk and GPU
  driver, and a hash of the report it describes. `weft-bench verify` checks it.
- Cross-engine reports record the source commit, the exact command line, and where each engine
  wrote its files.
- Real corpora (BTC closes, UCI household power) are not redistributed. Every workload that reads
  one takes a path, so you can run it on your own data.

**Statistics.**

- Each figure is a distribution over repetitions (p50, p95, p99, mean, standard deviation), not a
  single run.
- Bootstrap confidence intervals are reported beside it, seeded from the dataset seed.
- The first repetition is reported separately from the warm ones, because first-touch memory
  inflates it.
- In the cross-engine honesty section (EVI-15R), a comparison counts as a win or a loss only when
  the two p50 intervals do not overlap. Overlap is reported as noise, not as a small win. The
  nightly regression gate (EVI-5) is stricter: it needs both a move past its threshold (10% by
  default) and disjoint intervals.

**Fair comparison.**

- Comparisons pair results on profile, adapter, workload and dataset shape. A changed seed or
  point count is a different workload, not a slower one.
- Adapters report what produced their numbers: engine, method, whether a native engine backs them,
  and whether the requested spline was actually run. The baselines say they ignore it.
- Other engines are measured only where their licences allow publishing results, under six rules:
  - the licence is read and recorded;
  - one reproducible script runs the comparison;
  - each engine is tuned per its own documentation;
  - each vendor gets 14 days to preview the results;
  - losses are published;
  - corrections are logged.
- kdb+, AVEVA PI, and the enterprise editions of QuestDB and InfluxDB 3 are excluded on licence
  grounds (`WILLINGNESS-TO-PAY.md`, *Benchmark-publication policy*).

**What is not yet measured.** The cross-engine report covers DuckDB `DOUBLE` on load and linear
interpolation. The DuckDB `DECIMAL` arm, downsampling, storage bytes, the UCI corpus at three
scales, and a run on dedicated reference hardware are still to come (EVI-14 and EVI-15). Until
then, no cross-engine figure is a headline.

**Kept negative results.** The bit-sliced value codec, the GPU on stored interpolation, splines on
prices, and the approximate percentile were all measured slower or no better than the obvious
alternative. They stay in the README as results, not footnotes.

---

## Interpolation-accuracy methodology

Speed figures say how fast a grid was filled. These say how right it was.

**Synthetic ground truth.** The synthetic profiles sample an analytic signal at irregular,
jittered instants, add bounded noise, and drop samples. The available shapes are `MultiSine`,
`Sawtooth`, `Step` and `DampedSine`. Because the signal is a formula, the true value at every
grid instant is known exactly. Each reconstruction is scored against it with:

- **RMSE**, which punishes large misses;
- **MAE**, the typical miss;
- **maximum absolute error**, the worst instant;
- **bias**, whether the method leans high or low.

All four are reported together. Each hides something the others show: a symmetric error cancels
in bias but not in MAE.

**Missingness patterns.** Missing completely at random (each sample dropped independently) and
block missingness (contiguous outages) are separate datasets. A method that does well on scattered
gaps can fail across an outage.

**Real held-out values.** `weft-bench --gap-fill --gf-csv` cuts outages out of a real series and
scores each fill against the real values it removed. This is where the spline-versus-linear result
on prices came from. A real smooth-signal corpus is the open item before WeftDB makes any "spline
is more accurate" claim on real data. The candidates, all openly licensed, are listed in
`DESIGN-PARTNERS.md`.

**Point provenance.** Every reconstructed point is marked raw, interpolated or extrapolated, so an
accuracy figure can be split by what was measured and what was inferred, and so can a downstream
system.

**Extrapolation rule.** Outside the data, `Linear` and `Quadratic` extend the first or last
segment's curve, `Cubic` holds the edge value, and a `Polynomial` can be clamped by a bounds
factor (splimes' `Spline` documentation). The cross-engine DuckDB arm is written to the same linear
rule and gated on it, so edge behaviour is compared like for like.

---

## Precision and physical value types

**The guarantee is about storage, not arithmetic.** WeftDB's logical type is `BigDecimal`. Each
stored aspect declares a physical encoding (`F64`, `F32`, `ScaledI64`, `ScaledI128`, `Decimal128`
or `BigDecimalText`), a permitted per-value error, and a timestamp unit. A value that cannot be
stored within that bound is **rejected** with `SealError::Encode` or
`SealError::ToleranceExceeded`, which is a `400` over HTTP. It is never rounded silently. With
`scale: 2` and tolerance `0`, a third decimal place is an error at ingest.

**What "exact" covers.**

- **Stored and raw values** come back exactly as stored. A raw point in an interpolation result is
  the stored decimal itself.
- **Reductions** are exact: `avg`, `sum`, the percentiles and the time-weighted averages are
  computed in integer or `BigDecimal` arithmetic.
- **Interpolated and extrapolated values are not.** splimes computes every method in `f64`, and its
  `BigDecimal` output is the shortest decimal that round-trips each computed `f64`
  (`weftdb/src/types/interpolate_range.rs`). An interpolated value is therefore as precise as an
  `f64` computation, about 15–17 significant digits, and no more. The `f64` output path carries the
  same information for those points, at a fraction of the cost.
- **Approximate percentiles** (`sketch_p50` … `sketch_p99`) are named as approximate, and they
  are within 1% of the exact answer (README, *Compute endpoints*).

**What exactness costs.** Measured against an `f64` loop on the same data, hourly `avg` over 1M
real BTC closes costs:

| Path | Cost vs `f64` |
| --- | --- |
| Integer path on stored `ScaledI64` mantissas | about 2.1× |
| `BigDecimal` points | about 10× |

QuestDB documents about 2× for its `DECIMAL`, so exact reductions are roughly at parity, not
ahead. Building a `BigDecimal` per interpolated output point is most of an exact interpolation's
cost: 38.0 ms against 7.7 ms for the `f64` call on about 1M points.

**Choosing a physical type.**

- `recommend_encoding` picks the narrowest encoding that holds a column within its tolerance, and
  the benchmark reports the bytes per point it would cost before anything is written.
- `F64` and `F32` are exact only for binary-representable values; `0.1` is lossy, and the residual
  is measured.
- `ScaledI64` and `ScaledI128` are exact within their scale and are the fast integer path.
- `Decimal128` carries a per-value scale up to 38 digits.
- `BigDecimalText` is the lossless escape hatch.
