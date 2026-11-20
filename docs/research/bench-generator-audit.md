# Generator audit — corpus shape flattery in the remaining weft-bench workloads

**Issue #396.**  
**Author:** slice BRIEF-396.  
**Date:** 2026-11-20.  
**Branch:** `p396/generator-audit`.

---

## Method

Issue #395 ([`benchmark-hygiene`](/ROADMAP.md#L236)) established that the synthetic ingest corpus
(produced by `InterpolationProfile::generate_synthetic`) flattered WeftDB because its shape — f64
binary-expansion values, sinusoidal signal, controlled irregularity — was a better fit for the
engine than any real corpus would be.  Issue #398 found the same mechanism behind the ~2.6×
`sketch_p99` speed-up claim.  *Flattery* in this audit means: the generator's default parameters
produce a shape that is either (a) unrealistically compressible or (b) unrealistically
fast to decode/reduce, compared to real-world telemetry or financial data.

The audit checks every `weft-bench` generator for four properties:

1. **Value cardinality and distribution** — real telemetry has heavy tails, outliers, and a
   wide dynamic range; 2-decimal clustered values or clean sinusoids compress better.
2. **Timestamp regularity** — perfectly regular 1-second or 10-millisecond strides let the engine
   use closed-form timestamp resolution (skipping decode entirely).
3. **Value precision** — very short fixed-point scales (2 decimal places) or exact f64 binary
   expansions both produce different memory/compression behaviour than realistic 4–8-digit
   measurements.
4. **Block/pattern structure** — repeating 97-cycle values, fixed spacing, and every cell
   present (or a known density) are not what real data looks like.

Each generator's default shape is read from source.  Every file:line citation is from
`origin/main` at `51ba63d`, re-verified at `1de5435`: neither `bench/claims.json` nor the cited
`weft-bench` lines moved between those commits, so the `L…` ranges below are the objects in the
registry as it stands.

---

## Generator table

| Generator (file:line) | Shape it produces (with the defaults that produce it) | Flattery classes shared with the ingest corpus | Workloads consuming it |
|---|---|---|---|
| `profile.rs:393` [`generate_synthetic`] via `SyntheticParams::default()` (:190); also `synthetic()` (:267), `generate()` (:384) | ~480 irregular points over 1 day; 20% drop rate; ±2.0 noise; MultiSine; f64 binary expansion (`value_decimals: None`). | **f64 binary expansion values** (maximum dynamic range, no rounding); clean analytic signal; controllably irregular jitter — the #395 corpus. | `weft-bench --synthetic` (interpolation); accuracy measurement (`measure_accuracy`); `interp-latency` (generated arm). |
| `compression.rs:153` [`generate`] via `CompressionParams::default()` (:109) | 20,000 **regular** rows; **2-decimal** `10000000.{i%97:02d}` clustered (FOR) values. | **2-decimal scale** (compresses better than real multiscale data); **regular timestamps** (closed-form stride); **97-cycle repetition** (FOR-friendly). | `weft-bench --compression` (bench `compression`). |
| `downsample.rs:146` [`generate`] via `DownsampleParams::default()` (:75) | 60,000 samples at **1-second regular** stride (~16.7 h); **2-decimal** sinusoidal + noise values; per-minute bucket reduction. | **2-decimal scale**; **perfectly regular timestamps** (1 s stride); clean sinusoidal signal (low noise, no outliers). | `weft-bench --downsample` (bench `downsample`); claim `reductions-2026-10-09`. |
| `gap_fill.rs:157` [`generate`] via `GapFillParams::default()` (:70) | 86,400 points at **1-second regular** stride (full day); 20% bucket-loss outages averaging 5 min; **2-decimal** `20·sin(2π·t/3600)+50` values. | **2-decimal scale**; **regular 1 s timestamps**; clean sinusoidal truth (no multi-scale noise). | `weft-bench --gap-fill`. |
| `point_lookup.rs:182` [`generate`] via `PointLookupParams::default()` (:101) | 20,000 **regular** rows (10 ms stride); values `10000000.{i%97:02d}` (2-decimal, 97-cycle). | **2-decimal scale**; **perfectly regular timestamps**; **97-cycle repetition** (FOR codec). | `weft-bench --point-lookup`. |
| `range_fetch.rs:138` [`generate`] via `RangeFetchParams::default()` (:71) | 20,000 **regular** rows (10 ms stride); same `10000000.{i%97:02d}` scheme. | **2-decimal scale**; **regular timestamps**; **97-cycle repetition** (FOR). | `weft-bench --range-fetch`. |
| `sparse.rs:121` [`generate`] via `SparseParams::default()` (:76) | 16 series × 256 blocks × 128 rows = 524,288 cells; ~20 % populated (~104,858 values); values from an analytic `signal(series, row)` formula that is rounded to 2 decimals by `round2` (`sparse.rs:40`, called at `:49`). | **2-decimal scale** (the same rounding as the ingest corpus); **clean analytic signal**; **uniform random density** (no real-world locality/correlation). | `weft-bench --sparse`. |
| `e2e_interpolation.rs:337` [`synthetic_corpus`] | Seed-controlled; sinusoidal f64 values at **3 decimal places**; irregular jittered timestamps; length sized to cover the largest test window. | **3-decimal rounding** (short scale); clean sinusoidal signal; jittered timestamps. | `e2e` interpolation tests (not a named bench). |
| `accuracy.rs:136` [`synthetic_ground_truth`] | Evaluates `InterpolationProfile::clean_signal_at` on the reconstruction grid. No independent shape — delegates to the profile's `signal_shape`. | Inherits whatever `InterpolationProfile` provides. | Accuracy scoring (`measure_accuracy`). |
| Real-corpora entry points: `uci.rs:89` ([`load_uci_corpus`]) and `line_protocol.rs` | UCI household power consumption (semicolon-separated, real `Global_active_power` values loaded as exact `BigDecimal`). Line-protocol payloads (any real `.lp` file). | **No flattery** — these are real or externally-generated datasets. | `weft-bench --csv` (point-lookup, range-fetch); `--uci`; `--line-protocol`. |
| Size presets: `scale.rs` [`ScalePreset`] | `Tiny`, **`Small`** (86,400 = "a day of one-second telemetry per series", the default), `Medium` (604,800), `Large` (2,592,000). | Multiplier of row counts — does not introduce or mitigate shape flattery on its own. | Every workload that accepts `--scale`. |

---

## Claims this implicates

Every claim in `bench/claims.json` whose measurement runs on a synthetic weft-bench generator
is assessed below.  Claims based on the BTC corpus or real data are `unaffected`.

| Claim ID | Line in `bench/claims.json` | Workload | Generator | Verdict | Reasoning |
|---|---|---|---|---|---|
| `pointread-2026-10-08` | L5–17 | point-read | `weft-physical-type/benches/pointread.rs` (not weft-bench) | **unaffected** | Uses its own generated columnar segment, not the weft-bench generators. |
| `ingest-real-btc` | L18–30 | ingest | `btc_1min.csv` (real corpus) | **unaffected** | Real BTC closes, not synthetic. |
| `reductions-2026-10-09` | L31–43 | downsample | `DownsampleProfile` (downsample.rs:146) | **suspect** | 60k regular 1-second points, 2-decimal sinusoid — the same 2-decimal regularity found flattering in #395. The dataset field already says "two-decimal generator". No re-run without a representative generator (cf. #397). |
| `exact-decimals-2026-10-11` | L44–56 | (accuracy context) | "two-decimal generator" | **suspect** | Same 2-decimal synthetic shape; already noted as not reproducible. |
| `reduce-scaled-2026-10-09` | L57–69 | downsample (scaled) | "two-decimal generator" | **suspect** | Same shape family. |
| `decimal-path-before` | L70–82 | downsample (decimal path) | "two-decimal generator" | **suspect** | Same shape family. |
| `serve-reduce-2026-10-09` | L83–95 | server-side reduce | `btc_1min.csv` (real corpus) | **unaffected** | Real closes. |
| `serve-avg-before-after` | L96–108 | server-side avg | `btc_1min.csv` (real corpus) | **unaffected** | Real closes. |
| `interp-latency` | L109–121 | interpolation | `InterpolationProfile::generate_synthetic` (profile.rs:393) + BTC | **suspect** for the **0.58 ms** figure (generated arm); **unaffected** for the **6.4 ms** figure (BTC arm). The generated arm is the #395 corpus. |
| `sketch-claim-corrected` | L122–134 | sketch_p99 | "old generator (~50-digit values)" | **already-corrected** | Retracted in #398. |
| `backup-daemon-ticks` | L135–147 | backup daemon | Local store observation | **unaffected** | Not a bench; timing observation. |

**Headline:** 5 claims are `unaffected`, 4 are `suspect`, 1 is `already-corrected`, and
`interp-latency` splits — its generated arm is `suspect`, its BTC arm `unaffected` — which is how
the tally of 5 suspect is reached.  All 5 suspect
claims rest on the same 2-decimal regular generator family (downsample / point-lookup / range-fetch
/ gap-fill / interpolation) whose defaults produce the shape properties the earlier audits found
flattering.

---

## What would close this

Issue [#397](https://github.com/basic-automation/weftdb/issues/397) asks for *either* a
representative storage generator (one that produces real-seeming timestamps, multi-scale decimals,
and multi-modal value distributions) *or* real-corpus defaults.  The single change that would
retire the most flattery classes is:

> **Replace the default `value_decimals: Some(2)` in `DownsampleParams::default()` and the
> analogous 2-decimal defaults in `PointLookupParams::default()` and
> `RangeFetchParams::default()` with `value_decimals: None` (f64 binary expansion), and give
> `CompressionParams` the same knob (its 2-decimal clustering is fixed by `ValueShape::Clustered`,
> `compression.rs:106`, not by a `value_decimals` field) — and set `regular: false` as the default
> for at least one of them.**

That would eliminate the **2-decimal scale** class (4 generators → made irrealistic) and the
**perfectly regular timestamps** class from the default profiles.  The downsample workload's
default feeds the `reductions-2026-10-09` figure; changing its default would shift that claim's
basis.  No implementation in this PR — #397 remains open.

---

## Not established by this audit

- **Magnitude of the flattery.**  Without a re-run, the audit can only say *that* a shape is
  irrealistic, not *how much* it inflates the figures.  #395 measured ~2.5× for the ingest corpus;
  the downsample/interpolation workloads could be different.
- **Which suspect figures would survive a real-corpus rerun.**  The 6.4 ms `interp-latency` arm
  (BTC) already avoids flattery; the 0.58 ms arm may shrink or stay.
- **The effect of multi-scale decimals** (values like `1.234` alongside `123456.78`) on the FOR
  codec's compression ratio — the existing 2-decimal clustered values are the FOR sweet spot, but
  no bench exercises a mixed-scale column.
- **Whether the sparse generator's uniform random density hides real-world locality.**
  Real sparse timeseries often cluster activity in bursts; the uniform-per-block model may
  overstate scan efficiency.