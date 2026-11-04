# GPU economics (roadmap 5.5)

What GPU interpolation costs next to running the same interpolation on the CPU alone: cold
start, the CPU-only fallback, GPU utilization, behaviour under concurrent callers, and
dollars per billion interpolated points on public cloud instances. The numbers come from
`weft-bench --gpu-economics` (schema v29, `gpu_economics` block). The prices were read on
**2026-11-04**.

> **Status: measurement pending.** Every value marked `⟦MEASURE: …⟧` is filled by the
> measurement run (commands below). The instance prices, the formulas and the break-even
> ratios do not depend on that run and are final as of 2026-11-04.

## What was measured, and where

| | |
|---|---|
| GPU | NVIDIA GeForce RTX 4070 Ti SUPER (Vulkan), `f64` and `f32` shaders |
| CPU | AMD Ryzen 9 7950X3D, 16 cores (SMT off), `Backend::Cpu` per caller and `Backend::Parallel` (rayon) |
| Software adapter | lavapipe was **not** installed on the measurement host (no `lvp_icd` in `/usr/share/vulkan/icd.d/` on 2026-11-04), so it was not measured |
| splimes | the version in `Cargo.lock` at the measured commit ⟦MEASURE: version⟧ |
| Commit | ⟦MEASURE: commit⟧, release build, `--locked` |
| Workload | cubic, one-second grid, 1 Mi output points per call from 10,000 irregular knots, `run_f64` |

**Not measured: AMD, Intel and Apple GPUs.** The project has no such hardware attached
(the workstation's Radeon iGPU is bound to `vfio-pci`, see the splimes roadmap's
conformance-matrix item). Nothing below says anything about them, and none of the cloud GPUs
priced below were run either: their rows apply the local RTX 4070 Ti SUPER's throughput as a
stand-in, which is only an order-of-magnitude guide (see "Caveats").

## Reproduce

```bash
export CARGO_BUILD_JOBS=4
cargo run --release --locked -p weft-bench -- --gpu-economics --out-dir OUT
```

The run writes one JSON report per lane (`gpu-economics-cold-prewarm`, `-cold-lazy`,
`-no-adapter`, `-fallback-<backend>`, `-contention-<backend>-c<N>`). Knobs: `--ge-points`,
`--ge-knots`, `--ge-calls`, `--ge-lanes`, `--ge-concurrency`, `--ge-calls-per-caller`,
`--ge-contention`, `--ge-cold-spawns`, `--ge-no-adapter`, `--ge-smi-ms` (`weft-bench --help`).
The GPU is shared on the project machine: check the `baseline_*` utilization fields before
reading any GPU number (another process at 100% utilization makes every GPU row a contention
measurement).

## Cold start

A fresh child process per sample, timed from the parent's spawn to the child holding its
first GPU result (`f64`, 1 Mi points). `prewarm` starts the GPU first (`splimes::prewarm_gpu`:
adapter, device, kernel compilation); `lazy` makes the first `Backend::Gpu` call directly.

| | prewarm | lazy |
|---|---|---|
| spawn → `main` (median) | ⟦MEASURE⟧ | ⟦MEASURE⟧ |
| device + kernels (`prewarm_gpu`, median) | ⟦MEASURE⟧ | — |
| first call (median) | ⟦MEASURE⟧ | ⟦MEASURE⟧ (includes opening the device) |
| warm call (median) | ⟦MEASURE⟧ | ⟦MEASURE⟧ |
| **spawn → first GPU result** p50 / max | ⟦MEASURE⟧ | ⟦MEASURE⟧ |
| first child vs the rest (driver shader cache) | ⟦MEASURE⟧ | ⟦MEASURE⟧ |

## CPU-only fallback

The same call on every backend (30 timed calls each).

| Backend | p50 | p95 | p99 | points/s | max \|diff\| vs `Cpu` |
|---|---|---|---|---|---|
| `Cpu` (one thread) | ⟦MEASURE⟧ | ⟦MEASURE⟧ | ⟦MEASURE⟧ | ⟦MEASURE⟧ | — |
| `Parallel` (rayon, 16 cores) | ⟦MEASURE⟧ | ⟦MEASURE⟧ | ⟦MEASURE⟧ | ⟦MEASURE⟧ | ⟦MEASURE⟧ |
| `Gpu` `f64` | ⟦MEASURE⟧ | ⟦MEASURE⟧ | ⟦MEASURE⟧ | ⟦MEASURE⟧ | ⟦MEASURE⟧ |
| `Gpu` `f32` | ⟦MEASURE⟧ | ⟦MEASURE⟧ | ⟦MEASURE⟧ | ⟦MEASURE⟧ | ⟦MEASURE⟧ |
| `Auto` (ran on: ⟦MEASURE⟧) | ⟦MEASURE⟧ | ⟦MEASURE⟧ | ⟦MEASURE⟧ | ⟦MEASURE⟧ | ⟦MEASURE⟧ |

**With no adapter at all** (a child with every Vulkan driver and EGL vendor hidden):
adapter hidden: ⟦MEASURE: yes/no⟧; `prewarm_gpu` answered in ⟦MEASURE⟧ with ⟦MEASURE: error⟧;
a `Backend::Gpu` call failed with ⟦MEASURE: error⟧ (it must fail, not fall back);
`Backend::Auto` ran on ⟦MEASURE: backend⟧ and returned the full grid: ⟦MEASURE: yes/no⟧.
A CPU-only deployment therefore loses only throughput, never answers.

## Utilization and contention

1, 2, 4, 8 and 16 callers released together, 10 calls each. Throughput is output points over
the wall time; latency is per call. Utilization is `nvidia-smi` `utilization.gpu` (the share of
time a kernel was running, device-wide, every process) sampled every 100 ms, beside the
baseline before any GPU work.

| Callers | GPU `f64` points/s | GPU p50 / p95 / p99 | GPU util. mean / max (baseline) | CPU (`Cpu` per caller) points/s | CPU p50 / p95 / p99 |
|---|---|---|---|---|---|
| 1 | ⟦MEASURE⟧ | ⟦MEASURE⟧ | ⟦MEASURE⟧ | ⟦MEASURE⟧ | ⟦MEASURE⟧ |
| 2 | ⟦MEASURE⟧ | ⟦MEASURE⟧ | ⟦MEASURE⟧ | ⟦MEASURE⟧ | ⟦MEASURE⟧ |
| 4 | ⟦MEASURE⟧ | ⟦MEASURE⟧ | ⟦MEASURE⟧ | ⟦MEASURE⟧ | ⟦MEASURE⟧ |
| 8 | ⟦MEASURE⟧ | ⟦MEASURE⟧ | ⟦MEASURE⟧ | ⟦MEASURE⟧ | ⟦MEASURE⟧ |
| 16 | ⟦MEASURE⟧ | ⟦MEASURE⟧ | ⟦MEASURE⟧ | ⟦MEASURE⟧ | ⟦MEASURE⟧ |

Device memory at the end of the lanes: ⟦MEASURE⟧ MiB for this process
(`nvidia-smi --query-compute-apps`, when it lists a Vulkan process), ⟦MEASURE⟧ MiB device-wide.

## Cost per billion interpolated points

**Formula.** An instance at `P` dollars per hour that sustains `T` output points per second
costs

    $ per 10⁹ points = P / (T × 3600) × 10⁹

**Prices** (AWS EC2, US East (N. Virginia), Linux, shared tenancy, on demand), from AWS's
public price list for that region and OS
(`https://b0.p.awsstatic.com/pricing/2.0/meteredUnitMaps/ec2/USD/current/ec2-ondemand-without-sec-sel/US%20East%20(N.%20Virginia)/Linux/index.json`,
publication date 2026-11-03T23:57:16Z, read 2026-11-04; the human-readable page is
<https://aws.amazon.com/ec2/pricing/on-demand/>). The g6.xlarge, g5.xlarge and c7a.4xlarge
figures match the on-demand prices on <https://instances.vantage.sh/aws/ec2/g6.xlarge>,
<https://instances.vantage.sh/aws/ec2/g5.xlarge> and <https://instances.vantage.sh/aws/ec2/c7a.4xlarge>
on the same day:

| Instance | What it is | $/hour | GPU throughput needed to match `c7a.4xlarge` |
|---|---|---|---|
| c7a.4xlarge | 16 vCPU AMD EPYC 9R14 (one vCPU per core), 32 GiB, CPU only | 0.82112 | — (the CPU baseline) |
| c7i.4xlarge | 16 vCPU Intel (8 cores, 2 threads each), 32 GiB, CPU only | 0.714 | — |
| g6.xlarge | 1× NVIDIA L4 24 GB, 4 vCPU, 16 GiB | 0.8048 | 0.98 × the c7a's throughput |
| g5.xlarge | 1× NVIDIA A10G 24 GB, 4 vCPU, 16 GiB | 1.006 | 1.23 × |
| g6e.xlarge | 1× NVIDIA L40S 48 GB, 4 vCPU, 32 GiB | 1.861 | 2.27 × |
| p5.4xlarge | 1× NVIDIA H100 80 GB, 16 vCPU, 256 GiB | 6.88 | 8.38 × |

The last column is the price ratio, so it needs no measurement: a GPU instance is cheaper per
point only if it interpolates that many times faster than the 16-core CPU instance.

**What the price alone implies** (a scale for reading the measured rows; not a measurement):

| Sustained throughput | c7a.4xlarge | c7i.4xlarge | g6.xlarge | g5.xlarge | g6e.xlarge | p5.4xlarge |
|---|---|---|---|---|---|---|
| 10 M points/s | $0.0228 | $0.0198 | $0.0224 | $0.0279 | $0.0517 | $0.1911 |
| 100 M points/s | $0.00228 | $0.00198 | $0.00224 | $0.00279 | $0.00517 | $0.0191 |
| 1 G points/s | $0.000228 | $0.000198 | $0.000224 | $0.000279 | $0.000517 | $0.00191 |

**With the measured throughputs** (local hardware standing in for the cloud hardware; best
contention step for each):

| | Local throughput used | Instance | $ per 10⁹ points |
|---|---|---|---|
| CPU only | ⟦MEASURE: CPU points/s at 16 callers⟧ | c7a.4xlarge | ⟦MEASURE⟧ |
| GPU `f64` | ⟦MEASURE: GPU f64 points/s, best step⟧ | g6.xlarge | ⟦MEASURE⟧ |
| GPU `f64` | same | g5.xlarge | ⟦MEASURE⟧ |
| GPU `f32` | ⟦MEASURE: GPU f32 points/s (fallback lane)⟧ | g6.xlarge | ⟦MEASURE⟧ |

Verdict: ⟦MEASURE: which is cheaper per point, by how much, and at what concurrency⟧.

## Caveats

- **The cloud rows are arithmetic on local throughput.** None of the priced instances was
  run. An RTX 4070 Ti SUPER is not an L4, A10G, L40S or H100; their memory bandwidth, clocks
  and `f64` rates differ.
- **`f64` on these GPUs.** The RTX 4070 Ti SUPER, like the L4, A10G and L40S, runs `f64` at a
  small fraction of its `f32` rate (1/64 on these consumer/graphics-class parts, per NVIDIA's
  architecture whitepapers); the H100 runs it at half its `f32` rate. So the local `f64` row is a
  fair stand-in for the L4/A10G/L40S class at best, and understates an H100 by a wide margin.
  The `f32` row is within splimes' published `f32` tolerance (1e-5 · range · Λ), not `f64`'s.
- **CPU work on a 4-vCPU GPU instance.** splimes prepares large inputs and assembles the output
  on the CPU (rayon), and the local GPU rows had 16 cores for that. g5/g6/g6e `.xlarge` have 4
  vCPUs, so their GPU path would likely be slower than the local one.
- **On-demand, one region, compute only.** No reserved, savings-plan or spot prices, no
  storage, network or egress, and no idle time: a GPU instance billed for a server that
  interpolates a few percent of the time costs its full hourly price.
- **Shared GPU.** The measurement host's GPU is shared with other work; the baseline
  utilization in each report says how busy it was.
