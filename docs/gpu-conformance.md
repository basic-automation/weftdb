# GPU conformance and tolerances (roadmap 5.6)

What WeftDB guarantees about the numbers its interpolation and extrapolation return on
every backend (`Cpu`, `Parallel`, the GPU in `f64` and in `f32`), and the measured matrix
behind it: per adapter, per method, the error against an exact reference, run-to-run
determinism, and drift between adapters.

The honesty rule of this page is the one `docs/RPO-RTO.md` uses: every figure is
**measured** (by the command given here), **published upstream** (splimes' own contract and
its `BENCHMARKS.md`, cited), or **labelled unmeasured**. The tables below are placeholders
until the measurement run fills them; a placeholder is not a result.

## The tolerances WeftDB guarantees

WeftDB interpolates with [splimes](https://github.com/basic-automation/splimes) and adopts
its numerical contract unchanged. Write `exact` for the method evaluated in exact arithmetic
on the input values rounded to their nearest `f64`, `range` for the spread of the input
values (`max − min`), `Λ(t)` for the Lebesgue function of the window used at `t` (1 for
linear interpolation inside the data; larger with irregular spacing, high degree and
distance outside the data) and `ε = 2⁻⁵²`. Every returned value `v` satisfies

```
|v − exact| ≤ bound · range · Λ(t) + ε · |exact|
```

| Backend | Precision | `bound` |
|---|---|---|
| `Cpu`, `Parallel` | `f64` | 1 × 10⁻¹³ |
| GPU (any adapter with `SHADER_F64`) | `f64` | 1 × 10⁻¹³ |
| GPU (any adapter) | `f32` | 1 × 10⁻⁵ |

for every method (`Linear`, `Quadratic`, `Cubic`, `Polynomial(d, b)`), inside and outside
the data. Beyond the bound:

- **Raw points are exact.** A grid point that coincides with an input returns that input's
  value, digit for digit, on every backend and precision.
- **`Cpu` and `Parallel` are bit-identical**, and deterministic from run to run.
- **Non-finite results are errors, never values.** A result with no finite `f64` is a
  `NonFiniteResult` error (the API's `not_finite`), never a substitute value.
- **`f32` on the GPU hands back what it can't compute reliably.** Values that overflow, and
  windows whose knot gaps differ from the mean spacing by more than 1,024×, are recomputed in
  `f64` on the CPU, so the `f32` bound still holds.

What WeftDB does **not** guarantee:

- **GPU results bit-identical to the CPU's.** The GPU in `f64` is usually bit-identical
  (splimes: "usually"), but different hardware, drivers and shader compilers may contract or
  reorder floating-point operations differently. The guarantee across adapters is the bound,
  not the bits. The drift table below measures how far apart they actually land.
- **GPU run-to-run bit-identity.** It is measured below per adapter; until a measurement on
  each supported adapter shows it, it is an observation, not a promise.
- **The bound for subnormal results.** splimes' bound has no underflow term: where the exact
  result is subnormal, even the correctly rounded value can be up to 2⁻¹⁰⁷⁵ off, which the
  formula above does not allow for (splimes ROADMAP, "The contract's bound has no underflow
  term"). WeftDB's corpus contains no subnormal results.

Upstream evidence for these bounds: splimes' `tests/contract.rs` checks every backend and
precision against an exact 60-digit reference on 258 series, a two-million-knot series and
knots spanning chrono's whole range, and fails if a bound is exceeded. splimes'
`BENCHMARKS.md` publishes its worst results: NVIDIA RTX 4070 Ti SUPER `f64` ≤ 5.2e-16 and
`f32` ≤ 3.0e-7, lavapipe, WARP and Apple Metal `f32` ≤ 3.1e-7, all in units of
`range · Λ`.

## How WeftDB measures it

`weft-bench` ships a dedicated binary, `weft-gpu-conformance`
(`weft-bench/src/conformance.rs`, `weft-bench/src/bin/weft-gpu-conformance.rs`):

```
CARGO_BUILD_JOBS=4 cargo run -p weft-bench --release --locked --bin weft-gpu-conformance -- \
  matrix --size full --repeats 5 --out target/gpu-conformance
```

- **Targets.** `Cpu`, `Parallel`, GPU `f64` and GPU `f32`, each through both entry points
  WeftDB uses: `Interpolator::run` on `BigDecimal` points (the server's path) and
  `Interpolator::run_f64` (`interpolate_range`'s float path).
- **Adapters.** splimes' GPU is a process-wide singleton, so `matrix` runs one child process
  per adapter environment: `host` (the default, high-performance adapter, plus the CPU
  targets), `lavapipe` when Mesa's lavapipe Vulkan ICD is installed (selected with
  `VK_ICD_FILENAMES`), and any `--env NAME:VAR=VAL,...` given (for example
  `--env nv-gl:WGPU_BACKEND=gl` to try NVIDIA's GL path). An environment that can't run is
  listed as not measured, with the reason.
- **Methods.** `Linear`, `Quadratic`, `Cubic`, `Polynomial(4, 0.5)` (bounded),
  `Polynomial(5)` and `Polynomial(8)`.
- **Corpus.** Synthetic edge cases: 40-point random walks at microsecond, second, hour and
  30-day spacings (irregular, 0.05× to 3× the nominal gap), each as a plain walk, a 1e9
  offset with 1e-3 variation (the case that breaks naive single precision), ×1e-200 and
  ×1e200; one regular walk; 1 to 4 points (the methods step down); a constant series; a
  three-level step; shuffled duplicate instants; a 2,000-point walk; a dense burst (60×
  denser than its neighbours) and geometric gaps with alternating values (splimes' two stress
  shapes). And a real-data shape: 1,440 minutes in the shape of the UCI household-power
  corpus' `Global_active_power` (minute cadence, three decimals, a ~0.3 kW base load with
  appliance plateaus to ~8 kW, runs of missing minutes). `--uci FILE` adds the first
  `--uci-rows` rows (default 2,000) of the real file. Each series gets five 256-point grids
  per method: across the first edge, the middle, the densest stretch, across the last edge,
  and 100 edge spacings out.
- **Reference.** Each grid point is evaluated in exact 60-digit decimal arithmetic, which
  also computes `Λ(t)` (a port of splimes' `tests/common`). Errors are reported as
  `(|v − exact| − ε·|exact|)⁺ / (range · Λ(t))`, the unit the bound is published in, so a
  value ≤ `bound` passes.
- **Determinism.** Each target runs the corpus `--repeats` times; outputs are compared bit for
  bit against the first.
- **Drift.** Every pair of targets, across adapters too, is compared point by point: the share
  of bit-identical values and the largest difference, in the same unit.
- **Gate.** The command exits 1 if any target exceeds its bound, returns a raw point that is
  not the input, returns `NonFiniteResult` for a finite exact value or fails otherwise, or if
  `Cpu` and `Parallel` are not bit-identical. GPU run-to-run differences are reported, not
  failed (see above). `matrix.md` and `matrix.json` hold the full results.

`cargo test -p weft-bench --lib conformance` runs the same harness on a smoke corpus (and on
the GPU when one is present).

## Measured matrix

*Placeholder: filled by the measurement run (date, commit, splimes version, machine).*

### Environments

| Environment | Adapter | API | Type | Driver | f64 |
|---|---|---|---|---|---|
| host | _unmeasured_ | | | | |
| lavapipe | _unmeasured_ (not installed on the development workstation as of 2026-11-04: no `lvp_icd*.json`) | | | | |

### Accuracy (worst error, units of `range · Λ`)

| Method | `Cpu` | `Parallel` | GPU `f64` (NVIDIA) | GPU `f32` (NVIDIA) | Bound `f64` / `f32` |
|---|---|---|---|---|---|
| `Linear` | _unmeasured_ | _unmeasured_ | _unmeasured_ | _unmeasured_ | 1e-13 / 1e-5 |
| `Quadratic` | _unmeasured_ | _unmeasured_ | _unmeasured_ | _unmeasured_ | 1e-13 / 1e-5 |
| `Cubic` | _unmeasured_ | _unmeasured_ | _unmeasured_ | _unmeasured_ | 1e-13 / 1e-5 |
| `Polynomial(4, 0.5)` | _unmeasured_ | _unmeasured_ | _unmeasured_ | _unmeasured_ | 1e-13 / 1e-5 |
| `Polynomial(5)` | _unmeasured_ | _unmeasured_ | _unmeasured_ | _unmeasured_ | 1e-13 / 1e-5 |
| `Polynomial(8)` | _unmeasured_ | _unmeasured_ | _unmeasured_ | _unmeasured_ | 1e-13 / 1e-5 |

### Determinism and drift

| Comparison | Bit-identical | Max drift (`range · Λ`) |
|---|---|---|
| `Cpu` vs `Parallel` | _unmeasured_ (guaranteed: 100%) | |
| `Cpu` vs GPU `f64` | _unmeasured_ | |
| `Cpu` vs GPU `f32` | _unmeasured_ | |
| GPU `f64` run to run | _unmeasured_ | |
| GPU `f32` run to run | _unmeasured_ | |
| `run` vs `run_f64` (same target) | _unmeasured_ | |

## Not measured here

- **AMD, Intel and Apple-silicon GPUs.** Not available to WeftDB's measurement machine. The
  workstation's Radeon iGPU is bound to `vfio-pci` for VM passthrough, so Vulkan does not see
  it (splimes ROADMAP, 2026-10-09). splimes' CI covers lavapipe, and WARP and Metal when the
  hosted runners expose them; those are splimes' figures, not this harness's.
- **NVIDIA's GL path (`f32`).** With `WGPU_BACKEND=gl` opening the device fails on a headless
  Linux/NVIDIA setup (gfx-rs/wgpu#7068, per splimes' ROADMAP). `--env nv-gl:WGPU_BACKEND=gl`
  will record whether it still does. It needs a splimes release that honours `WGPU_BACKEND`
  and `WGPU_ADAPTER_NAME` (1.2.0 and later); the lockfile at the time of writing pins 1.0.0,
  where only `VK_ICD_FILENAMES` changes which adapter opens.
