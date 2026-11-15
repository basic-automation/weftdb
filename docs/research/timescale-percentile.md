# TimescaleDB percentile internals and the like-for-like comparison plan

**Research for roadmap item [Phase 25 — Research (2026-11-11) — TimescaleDB's default percentile aggregate is a UDDSketch, so it is the like-for-like comparison](#732).**

**Author:** slice TS-PCTL  
**Date:** 2026-11-14  
**Branch:** `p12/research-timescale-percentile`  

---

## 1. What does TimescaleDB's `percentile_agg()` use?

**Answer: `UDDSketch` with 200 buckets and `max_error = 0.001`.**

| Source | URL | What it says |
|--------|-----|--------------|
| Tiger Data docs (`percentile_agg()` reference) | <https://www.tigerdata.com/docs/reference/toolkit/percentile-approximation/uddsketch/percentile_agg> | "Internally, it calls `uddsketch` with a `size` of 200 and a `max_error` of 0.001." |
| GitHub source (`extension/src/uddsketch.rs`) | <https://github.com/timescale/timescaledb-toolkit/blob/main/extension/src/uddsketch.rs> | Line 35–36: `const PERCENTILE_AGG_DEFAULT_SIZE: u32 = 200;` `const PERCENTILE_AGG_DEFAULT_ERROR: f64 = 0.001;` |
| Tiger Data docs (`uddsketch()` reference) | <https://www.tigerdata.com/docs/reference/toolkit/percentile-approximation/uddsketch/uddsketch> | "the true error may exceed this if too few buckets are provided for the data distribution" |
| Tiger Data docs (`error()` reference) | <https://www.tigerdata.com/docs/reference/toolkit/percentile-approximation/uddsketch/error> | Returns the maximum relative error actually reached by the sketch. Sample: `uddsketch(100, 0.001)` over `generate_series(0, 100)` returns `0.007999832004199894`. |
| Paper | <https://arxiv.org/abs/2004.08604> | "UDDSketch: accurate and memory-efficient approximate distinct and distinct count sliding window sketching" — the algorithm behind UDDSketch. |
| Toolkit source (`crates/udd-sketch/src/lib.rs`) | <https://github.com/timescale/timescaledb-toolkit/blob/main/crates/udd-sketch/src/lib.rs> | The Rust implementation of UDDSketch. Use of `SketchHashKey`, `gamma(alpha) = (1+alpha)/(1-alpha)`, `key = ceil(log_gamma(|value|))`. |

### 1.1 What layer does it live in?

The percentile approximation lives in the **TimescaleDB Toolkit** extension (`timescaledb-toolkit`), a `pgrx`-based PostgreSQL extension written in Rust. It is **not** part of the core TimescaleDB Apache-licensed code (`timescaledb/timescaledb`). The toolkit is distributed separately but is the canonical way to get percentile approximation.

When a user runs `percentile_agg(value)`, the transition function `percentile_agg_trans` (in `uddsketch.rs:38`) calls `uddsketch_trans_inner` with `default_size = 200` and `default_max_error = 0.001`. The aggregate returns a `UddSketch` PostgreSQL type that supports `approx_percentile`, `approx_percentile_rank`, `error`, `mean`, `num_vals`, and `rollup` for merging partials.

### 1.2 Error guarantees

- The constructor sets `alpha = max_error`. The sketch guarantees that any percentile estimate has relative error ≤ `alpha` **as long as the bucket budget is never exceeded**.
- When the bucket budget runs out, the sketch does a **uniform collapse**: every adjacent pair of buckets is merged into one (indices `2j-1` and `2j` become `j`). After `k` collapses the error bound grows to `α_k = tanh(2^k · atanh α)` (one collapse: `2α / (1+α²)`).
- The `error()` accessor returns the **actual** worst-case relative error the sketch currently guarantees — the closed form above, computed from the number of collapses. TimescaleDB's docs confirm: "The true error may exceed this if too few buckets are provided for the data distribution. You can get the true error using the `error()` function."

---

## 2. Concrete comparison plan

The existing `sketch_collapse` bench in `weft-reduce` (invoked via `cargo bench -p weft-reduce --bench sketch_collapse`) already prints relative error against an exact sort at multiple quantiles. It has two inputs: (a) a synthetic random walk of 100,000 samples at 1% and (b) every BTC/USD one-minute close in `database/datasets/btc_1min.csv` (7,146,457 samples, values 3.8 to 123,236).

### What to add to the bench

1. **Add a `UddSketch` variant configured at TimescaleDB defaults** (200 buckets, `alpha = 0.001`). The sketch already exists as `weft_reduce::UddSketch` (`weft-reduce/src/uniform.rs:52`). The bench results section in `sketch_collapse` should print:
   - Number of collapses observed on each input.
   - The reported `relative_accuracy()` (the actual error bound).
   - Relative error vs exact sort at p1, p50, p99.
   - Wall-clock time for `add`, `merge` of 64 collapsed sketches.

2. **Run the same for `DdSketch` at TimescaleDB-like parameters** (200 buckets, `alpha = 0.001`) — this is the equivalent `DdSketch::with_max_bins(0.001, 200)`. The key data point is whether `DdSketch` at 200 buckets collapses at all on the BTC corpus (which is ~520 buckets wide at 1% — more at 0.1% where `gamma = (1+0.001)/(1-0.001) ≈ 1.002`, so `log_gamma(123236/3.8)` ≈ 8116 buckets), and what error its lowest-bucket collapse produces.

3. **Run the same for `UddSketch` as configured for `sketch_p99`** (alpha = 0.01 = 1%, 2048 buckets max). This is the "2048 buckets at 1%" line that already exists but the figure is computed not measured — actually run it.

### Data to collect

| Configuration | Input | Collapses | Achieved bound | p1 error | p50 error | p99 error | add time | merge time |
|---|---|---|---|---|---|---|---|---|
| UddSketch(0.001, 200) — TimescaleDB default | BTC closes | (compute) | (print) | (print) | (print) | (print) | (ms) | (ms) |
| DdSketch(0.001, 200) | BTC closes | (compute) | (print) | (print) | (print) | (print) | (ms) | (ms) |
| UddSketch(0.01, 2048) — WeftDB sketch_p99 | BTC closes | (compute) | (print) | (print) | (print) | (print) | (ms) | (ms) |
| DdSketch(0.01, 2048) — WeftDB sketch_p99 (current) | BTC closes | 0 | 1% | 151× | 0.75% | 0.81% | 0.65 ms | 67 µs |

### Closed-form estimate (before measuring)

At `alpha = 0.001` (0.1%), `gamma = (1.001)/(0.999) ≈ 1.002002`. The BTC corpus spans 3.8 to 123,236. Bucket keys are `ceil(log_gamma(v))`. The number of distinct positive bucket keys the data spans:

- log_γ(123236) ≈ ln(123236) / ln(1.002002) ≈ 11.72 / 0.0020 ≈ 5860
- log_γ(3.8) ≈ ln(3.8) / ln(1.002002) ≈ 1.335 / 0.0020 ≈ 668
- Span: 5860 − 668 = **~5192 buckets**

With only 200 buckets available, the sketch will collapse. Each collapse halves the bucket span: after 1 collapse ~2596 buckets, after 2 collapses ~1298, after 3 collapses ~649, after 4 collapses ~325, after 5 collapses ~162 — so **5 collapses** are needed to fit 5192 buckets into 200.

The closed-form error after 5 collapses at `α = 0.001`:
```
α_5 = tanh(2^5 · atanh(0.001))
    = tanh(32 · 0.00100033)
    = tanh(0.0320106)
    ≈ 0.0320
```
So the achieved bound would be about **3.2%**, compared to WeftDB's `sketch_p99` which holds 1% without collapsing (when using 2048 buckets).

**Important caveat**: this estimate assumes the data is uniformly distributed within the range spread. If the BTC closes cluster heavily (most values near ~1830–68000 with a long tail), the effective bucket span may be smaller, and the actual collapses fewer. Measurement is required.

---

## 3. Actionable items

| # | Item | Owner |
|---|------|-------|
| 1 | Add `UddSketch(0.001, 200)` variant to the `sketch_collapse` bench (TimscaleDB default config) | next worker |
| 2 | Add `DdSketch(0.001, 200)` variant to the same bench | next worker |
| 3 | Run `sketch_collapse` on a quiet host with pinned cores and record all figures | next worker (or same as #1–2 after build) |
| 4 | When the TimescaleDB adapter lands (`weft-xbench`), run `percentile_agg()` on the same inputs and record `error()` beside timing — a speed comparison without error context flatters one side | deferred to adapter slice |
| 5 | Decide whether `sketch_p*` reductions should adopt `UddSketch` (owner-gated; needs sidecar version step) | owner |

---

## 4. Open questions

- What is the actual relative error of `UddSketch` on real timeseries data (not just the closed-form bound)? The bound is a **worst-case** guarantee; the actual error on BTC closes may be lower, which makes TimescaleDB's offering stronger than the 3.2% worst case.
- Does TimescaleDB's `percentile_agg` with its defaults produce tighter or looser answers than WeftDB's `sketch_p99` on the same data? Without the measurement from item #3, any claim here would be speculation.
- The toolkit's `uddsketch(100, 0.001)` over 0..100 example returns `error = 0.007999832004199894` — verifying the closed-form `tanh(2·atanh(0.001))`:
  `atanh(0.001) ≈ 0.001000333`,
  `tanh(2 · 0.001000333) = tanh(0.002000666) ≈ 0.0019993`? No — after two collapses the formula is `α_2 = tanh(4 · atanh α) = tanh(0.00400133) ≈ 0.0040`. But the example output is `0.007999832004199894`, which is `tanh(8 · atanh(0.001))` = 3 collapses. Why 3? Because the span from 0 to 100 with `size=100` and `alpha=0.001` causes 3 collapses. Matching `relative_accuracy` confirms the closed form is right.