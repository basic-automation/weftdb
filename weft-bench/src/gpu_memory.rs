//! GPU memory-stability workload — does the GPU buffer pool stay bounded and stable across
//! many interpolation calls of mixed sizes?
//!
//! splimes keeps idle device buffers in a size-tiered pool between calls (capped by
//! `GpuConfig::max_pool_bytes`, 512 MiB by default) so a repeated call reuses them instead of
//! allocating. A server runs interpolation calls of every size for as long as it is up, so
//! the pool must neither grow without bound nor thrash. This workload starts the GPU, then
//! runs `rounds` rounds of GPU interpolations (`Backend::Gpu`, cubic, `f64`), each round one
//! call per configured output size in a seeded order, and samples `splimes::gpu_pool_stats`
//! after every call.
//!
//! The correctness gate: every call returns its full grid of finite values, the idle pool
//! never exceeds its cap, and the idle bytes after the first round never exceed the first
//! round's peak (no growth once every size has been seen). The report's `gpu_pool` block
//! records the cap, the buffer sets created, reused and evicted, how many were created after
//! the first round (zero means every later call reused the pool; more means eviction churn),
//! and the peaks, beside the per-call latency. A smaller `--gm-pool-mib` shows the eviction
//! behaviour. It needs a hardware GPU with `f64` shaders: a CPU/software adapter is refused,
//! as the server does not calibrate onto one either, and so is a device without `f64`
//! support (such as the Apple Paravirtual device on a macOS CI runner).
//!
//! The pool counts only idle buffer sets; a leak outside it would not show there. So every
//! call also samples `splimes::gpu_memory_report` (splimes 1.3: the device allocator's live
//! and reserved bytes, on Vulkan and DX12), and, out of process, `nvidia-smi`'s
//! `memory.used` every `--gm-smi-ms` as a cross-check that sees driver allocations too.
//! `--gm-soak-secs` keeps running rounds until that long has passed, for a long soak. The
//! report's `gpu_pool.device_memory` block holds both trends, each cut into at most
//! [`TREND_WINDOWS`] windows (the max of each), and flags monotonic growth after the first
//! round as a leak; a device-allocator leak fails the run.

use std::time::{Duration as StdDuration, Instant};

use chrono::{DateTime, Duration, TimeZone, Utc};
use rand::{seq::SliceRandom, RngExt, SeedableRng};
use rand_chacha::ChaCha8Rng;
use serde::{Deserialize, Serialize};
use splimes::{Backend, GpuConfig, Interpolator, Resolution, Spline};

use crate::{
	engine::{is_software_adapter, CalibrationStatus, EngineMetadata}, schema::{BenchResult, ColdWarm, CorrectnessReport, DatasetMeta, TimingBreakdown, SCHEMA_VERSION}, stats::{BootstrapConfig, LatencyStats}
};

/// Workload class label recorded for the GPU memory workload.
const WORKLOAD_GPU_MEMORY: &str = "gpu_memory";

/// Default seed for GPU memory profiles.
pub const DEFAULT_GPU_MEMORY_SEED: u64 = 0x6B0_0D1E_5EED;

/// The tunable knobs for a GPU memory profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuMemoryParams {
	/// RNG seed for the knots and the per-round call order.
	pub seed: u64,
	/// Rounds; each makes one call per size.
	pub rounds: usize,
	/// Output grid sizes (points per call), each called once per round.
	pub sizes: Vec<usize>,
	/// Input knots per call (irregular, spread over the grid).
	pub knots: usize,
	/// The pool cap to start the GPU with, in MiB; `None` keeps splimes' default.
	pub pool_mib: Option<u64>,
	/// Keep running rounds (at least `rounds`) until this many seconds have passed; `None`
	/// runs exactly `rounds`.
	pub soak_secs: Option<u64>,
	/// `nvidia-smi` sampling interval in milliseconds; 0 turns the cross-check off.
	pub smi_interval_ms: u64,
}

impl Default for GpuMemoryParams {
	/// Twenty rounds of 16 Ki-, 1 Mi- and 4 Mi-point calls over 1,000 knots, default pool.
	fn default() -> Self {
		Self { seed: DEFAULT_GPU_MEMORY_SEED, rounds: 20, sizes: vec![16 << 10, 1 << 20, 4 << 20], knots: 1_000, pool_mib: None, soak_secs: None, smi_interval_ms: 1_000 }
	}
}

/// What the pool did over a run (the report's `gpu_pool` block).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuPoolSummary {
	/// The pool cap the GPU ran with, in bytes.
	pub max_pool_bytes: u64,
	/// Rounds run.
	pub rounds: usize,
	/// Interpolation calls made.
	pub calls: usize,
	/// Buffer sets created over the run.
	pub created: u64,
	/// Buffer sets created after the first round: zero when every later call reused one.
	pub created_after_first_round: u64,
	/// Calls that reused an idle buffer set.
	pub reused: u64,
	/// Buffer sets freed because the pool was over its cap.
	pub evicted: u64,
	/// The most idle bytes seen after any call of the first round.
	pub peak_idle_bytes_first_round: u64,
	/// The most idle bytes seen after any later call.
	pub peak_idle_bytes_later: u64,
	/// Idle bytes after the last call.
	pub final_idle_bytes: u64,
	/// No idle-byte growth after the first round, and never over the cap.
	pub stable: bool,
	/// The device's own memory over the run (schema v30); `None` in older artifacts.
	#[serde(default)]
	pub device_memory: Option<DeviceMemorySoak>,
}

/// The most windows a trend is cut into.
pub const TREND_WINDOWS: usize = 60;

/// Device memory over a `gpu_memory` run: splimes' allocator report sampled after every
/// call, and `nvidia-smi`'s `memory.used` sampled out of process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceMemorySoak {
	/// The soak length asked for, in seconds (`None`: a fixed number of rounds).
	pub soak_secs: Option<u64>,
	/// Wall-clock seconds the rounds took.
	pub elapsed_secs: u64,
	/// Calls after which the allocator gave a report (0: the backend has none).
	pub samples: usize,
	/// The most allocated bytes after any call of the first round.
	pub peak_allocated_bytes_first_round: u64,
	/// The most allocated bytes after any later call.
	pub peak_allocated_bytes_later: u64,
	/// Allocated bytes after the last call.
	pub final_allocated_bytes: u64,
	/// Reserved (block) bytes after the last call.
	pub final_reserved_bytes: u64,
	/// Allocated bytes at the end of each round after the first, cut into at most
	/// [`TREND_WINDOWS`] windows (each window's max).
	pub allocated_trend_bytes: Vec<u64>,
	/// `allocated_trend_bytes` never falls and ends higher than it starts: a leak.
	pub allocated_growth: bool,
	/// `nvidia-smi` `memory.used` per GPU (the whole GPU, other processes included).
	pub smi: Vec<SmiMemoryTrend>,
}

/// One GPU's `nvidia-smi` `memory.used` over the run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmiMemoryTrend {
	/// GPU index.
	pub gpu_index: u32,
	/// Samples taken.
	pub samples: usize,
	/// `memory.used` in MiB, cut into at most [`TREND_WINDOWS`] windows (each window's max).
	pub memory_used_trend_mib: Vec<u64>,
	/// The trend never falls and ends higher than it starts.
	pub growth: bool,
}

/// Cut `series` into at most `windows` consecutive windows of near-equal length and keep each
/// window's max.
#[must_use]
pub fn window_max(series: &[u64], windows: usize) -> Vec<u64> {
	if series.is_empty() || windows == 0 {
		return Vec::new();
	}
	let n = windows.min(series.len());
	(0..n).map(|w| series[w * series.len() / n..(w + 1) * series.len() / n].iter().copied().max().unwrap_or(0)).collect()
}

/// Monotonic growth: at least three points, never falling, and ending above the start.
#[must_use]
pub fn monotonic_growth(trend: &[u64]) -> bool {
	trend.len() >= 3 && trend.windows(2).all(|w| w[1] >= w[0]) && trend.last() > trend.first()
}

/// Per-GPU `memory.used` trends from `nvidia-smi` samples, in GPU order. The first window
/// (the start-up) is left out of the growth check.
#[must_use]
pub fn smi_trends(samples: &[crate::gpu_economics::SmiSample]) -> Vec<SmiMemoryTrend> {
	let mut indices: Vec<u32> = samples.iter().map(|s| s.index).collect();
	indices.sort_unstable();
	indices.dedup();
	indices.into_iter()
		.map(|gpu_index| {
			let used: Vec<u64> = samples.iter().filter(|s| s.index == gpu_index).map(|s| s.memory_used_mib).collect();
			let trend = window_max(&used, TREND_WINDOWS);
			// The first window holds the start-up allocations; growth is judged after it.
			let growth = monotonic_growth(trend.get(1..).unwrap_or_default());
			SmiMemoryTrend { gpu_index, samples: used.len(), memory_used_trend_mib: trend, growth }
		})
		.collect()
}

/// A reproducible GPU memory workload profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuMemoryProfile {
	/// Human-readable profile name, recorded in results.
	pub name: String,
	/// The knobs.
	pub params: GpuMemoryParams,
}

impl GpuMemoryProfile {
	/// Build a profile; at least one round, one size of at least two points, and two knots.
	#[must_use]
	pub fn new(name: impl Into<String>, params: GpuMemoryParams) -> Self {
		let mut sizes: Vec<usize> = params.sizes.into_iter().map(|n| n.max(2)).collect();
		if sizes.is_empty() {
			sizes = GpuMemoryParams::default().sizes;
		}
		Self { name: name.into(), params: GpuMemoryParams { rounds: params.rounds.max(1), knots: params.knots.max(2), sizes, soak_secs: params.soak_secs.filter(|s| *s > 0), ..params } }
	}
}

/// One call's input: knot instants and values.
type Knots = (Vec<DateTime<Utc>>, Vec<f64>);

/// The fixed grid start (2020-01-01T00:00:00Z).
fn anchor() -> DateTime<Utc> {
	Utc.timestamp_opt(1_577_836_800, 0).single().expect("valid fixed epoch anchor")
}

/// Seeded irregular knots spread over a one-second grid of `points` points.
fn knots(rng: &mut ChaCha8Rng, count: usize, points: usize) -> Knots {
	let span = i64::try_from(points - 1).unwrap_or(i64::MAX);
	let mut offsets: Vec<i64> = (0..count).map(|_| rng.random_range(0..=span)).collect();
	offsets.extend([0, span]);
	offsets.sort_unstable();
	offsets.dedup();
	let values = offsets.iter().map(|&o| 20.0_f64.mul_add((crate::num::i64_to_f64(o) / 600.0).sin(), 50.0) + rng.random_range(-1.0..=1.0)).collect();
	(offsets.into_iter().map(|o| anchor() + Duration::seconds(o)).collect(), values)
}

/// Elapsed nanoseconds since `since`, saturated into a `u64`.
fn span_ns(since: Instant) -> u64 {
	crate::num::u128_to_u64(since.elapsed().as_nanos())
}

/// Start the GPU (with the profile's pool cap) and describe it for the report.
///
/// # Errors
///
/// If the cap cannot be applied (the GPU already started with another), no GPU starts, the
/// adapter is a CPU/software one, or it has no `f64` shader support (the workload runs
/// `run_f64`, so such a device would fail every call).
pub fn start_gpu(profile: &GpuMemoryProfile) -> anyhow::Result<EngineMetadata> {
	if let Some(mib) = profile.params.pool_mib {
		splimes::configure_gpu(GpuConfig::DEFAULT.with_max_pool_bytes(mib << 20)).map_err(|e| anyhow::anyhow!("cannot set the GPU pool cap: {e}"))?;
	}
	let info = splimes::prewarm_gpu().map_err(|e| anyhow::anyhow!("the gpu-memory workload needs a GPU: {e}"))?;
	anyhow::ensure!(!is_software_adapter(&info.device_type, &info.name), "the gpu-memory workload needs a hardware GPU; {} is a CPU/software adapter", info.name);
	anyhow::ensure!(info.supports_f64, "the gpu-memory workload runs f64 shaders; {} has no f64 shader support", info.name);
	let gpu = format!("{} ({}, {}; f64 shaders: {})", info.name, info.api, info.device_type, if info.supports_f64 { "yes" } else { "no" });
	Ok(EngineMetadata::new(CalibrationStatus::Disabled, Some(gpu), Some("gpu-memory workload: Backend::Gpu forced, not calibrated".to_string()), splimes::auto_thresholds()).with_gpu_driver(&info.driver))
}

/// Run the workload on a started GPU ([`start_gpu`]) and return its [`BenchResult`].
///
/// Latency is per call (the sizes mixed); throughput is output points per second over all
/// calls.
///
/// # Errors
///
/// If the GPU is not started or a call fails.
pub fn run_gpu_memory(profile: &GpuMemoryProfile) -> anyhow::Result<BenchResult> {
	let p = &profile.params;
	let max_pool_bytes = splimes::gpu_config().max_pool_bytes;
	let run_start = Instant::now();
	let mut rng = ChaCha8Rng::seed_from_u64(p.seed);
	let inputs: Vec<(usize, Knots)> = p.sizes.iter().map(|&n| (n, knots(&mut rng, p.knots, n))).collect();
	let generation_ns = span_ns(run_start);
	let interpolator = Interpolator::new(Spline::Cubic, Resolution::Seconds).backend(Backend::Gpu);

	let stats = || splimes::gpu_pool_stats().ok_or_else(|| anyhow::anyhow!("the GPU is not started"));
	let before = stats()?;
	let sampler = crate::gpu_economics::SmiSampler::start(p.smi_interval_ms);
	let soak = p.soak_secs.map(StdDuration::from_secs);
	let rounds_start = Instant::now();
	let (mut dev_samples, mut dev_peak_first, mut dev_peak_later, mut dev_last) = (0_usize, 0_u64, 0_u64, None::<splimes::GpuMemoryReport>);
	let mut round_end_allocated: Vec<u64> = Vec::new();
	let (mut samples_ns, mut complete, mut finite) = (Vec::new(), true, true);
	let (mut peak_first, mut peak_later, mut created_after_first) = (0_u64, 0_u64, 0_u64);
	let mut output_points = 0_usize;
	let mut order: Vec<usize> = (0..inputs.len()).collect();
	let mut round = 0_usize;
	while round < p.rounds || soak.is_some_and(|d| rounds_start.elapsed() < d) {
		order.shuffle(&mut rng);
		let created_before_round = stats()?.created;
		for &i in &order {
			let (n, (timestamps, values)) = &inputs[i];
			let end = anchor() + Duration::seconds(i64::try_from(*n - 1).unwrap_or(i64::MAX));
			let t0 = Instant::now();
			let series = interpolator.run_f64(timestamps, values, anchor(), end).map_err(|e| anyhow::anyhow!("GPU interpolation of {n} points failed: {e}"))?;
			samples_ns.push(span_ns(t0));
			complete &= series.len() == *n;
			finite &= series.values().iter().all(|v| v.is_finite());
			output_points += series.len();
			let idle = stats()?.idle_bytes;
			let device = splimes::gpu_memory_report();
			if round == 0 {
				peak_first = peak_first.max(idle);
			} else {
				peak_later = peak_later.max(idle);
			}
			if let Some(d) = device {
				dev_samples += 1;
				if round == 0 {
					dev_peak_first = dev_peak_first.max(d.allocated_bytes);
				} else {
					dev_peak_later = dev_peak_later.max(d.allocated_bytes);
				}
				dev_last = Some(d);
			}
		}
		if round > 0 {
			created_after_first += stats()?.created - created_before_round;
			if let Some(d) = splimes::gpu_memory_report() {
				round_end_allocated.push(d.allocated_bytes);
			}
		}
		round += 1;
	}
	let rounds = round;
	let elapsed_secs = rounds_start.elapsed().as_secs();
	let smi = sampler.map(crate::gpu_economics::SmiSampler::stop).unwrap_or_default();
	let allocated_trend_bytes = window_max(&round_end_allocated, TREND_WINDOWS);
	let allocated_growth = monotonic_growth(&allocated_trend_bytes);
	let device_memory = DeviceMemorySoak { soak_secs: p.soak_secs, elapsed_secs, samples: dev_samples, peak_allocated_bytes_first_round: dev_peak_first, peak_allocated_bytes_later: dev_peak_later, final_allocated_bytes: dev_last.map_or(0, |d| d.allocated_bytes), final_reserved_bytes: dev_last.map_or(0, |d| d.reserved_bytes), allocated_trend_bytes, allocated_growth, smi: smi_trends(&smi) };
	let after = stats()?;
	let measured_ns = samples_ns.iter().copied().fold(0_u64, u64::saturating_add);
	let timing = TimingBreakdown { dataset_generation_ns: generation_ns, measured_ns, end_to_end_ns: span_ns(run_start) };
	let stable = peak_later <= peak_first && peak_first.max(peak_later) <= max_pool_bytes;
	let summary = GpuPoolSummary { max_pool_bytes, rounds, calls: samples_ns.len(), created: after.created - before.created, created_after_first_round: created_after_first, reused: after.reused - before.reused, evicted: after.evicted - before.evicted, peak_idle_bytes_first_round: peak_first, peak_idle_bytes_later: peak_later, final_idle_bytes: after.idle_bytes, stable, device_memory: Some(device_memory) };

	let correctness = CorrectnessReport { output_count_ok: complete && stable && !allocated_growth, expected_output_points: p.sizes.iter().sum::<usize>() * rounds, actual_output_points: output_points, values_finite: finite, result_sha256: None };
	let latency = LatencyStats::from_samples(&samples_ns);
	let latency_ci = Some(LatencyStats::bootstrap_cis(&samples_ns, &BootstrapConfig { seed: p.seed ^ 0x06B0_C0DE_u64, ..BootstrapConfig::default() }));
	let throughput_points_per_sec = if measured_ns > 0 { crate::num::usize_to_f64(output_points) / (crate::num::u64_to_f64(measured_ns) / 1e9) } else { 0.0 };
	Ok(BenchResult { schema_version: SCHEMA_VERSION, profile: profile.name.clone(), adapter: "weftdb".to_string(), workload: WORKLOAD_GPU_MEMORY.to_string(), reps: rounds, dataset: DatasetMeta { input_points: inputs.iter().map(|(_, (t, _))| t.len()).sum(), output_points, irregular: true, missingness_fraction: 0.0, seed: p.seed, signal_shape: None }, latency, latency_ci, throughput_points_per_sec, timing, correctness, accuracy: None, storage: None, cold_warm: ColdWarm::from_samples(&samples_ns), gpu_pool: Some(summary), adapter_info: None, growth: None, online: None, compressed_query: None, analytics: None, samples_ns: samples_ns.clone(), between_run: None, gpu_economics: None })
}

#[cfg(test)]
mod tests {
	use serial_test::serial;

	use super::*;

	#[test]
	fn profiles_clamp_their_knobs_and_knots_span_the_grid() {
		let profile = GpuMemoryProfile::new("gm", GpuMemoryParams { rounds: 0, sizes: vec![], knots: 0, ..GpuMemoryParams::default() });
		assert_eq!((profile.params.rounds, profile.params.knots), (1, 2));
		assert_eq!(profile.params.sizes, GpuMemoryParams::default().sizes);
		let mut rng = ChaCha8Rng::seed_from_u64(7);
		let (timestamps, values) = knots(&mut rng, 50, 10_000);
		assert_eq!(timestamps.first(), Some(&anchor()));
		assert_eq!(timestamps.last(), Some(&(anchor() + Duration::seconds(9_999))));
		assert!(timestamps.windows(2).all(|w| w[0] < w[1]), "distinct, ascending");
		assert_eq!(timestamps.len(), values.len());
	}

	#[test]
	fn window_max_keeps_each_windows_max_and_growth_needs_a_rising_trend() {
		assert_eq!(window_max(&[], 4), Vec::<u64>::new());
		assert_eq!(window_max(&[5, 1, 2], 10), vec![5, 1, 2]);
		assert_eq!(window_max(&[1, 9, 2, 3, 4, 8], 3), vec![9, 3, 8]);
		assert_eq!(window_max(&(0..1_000).collect::<Vec<u64>>(), TREND_WINDOWS).len(), TREND_WINDOWS);
		assert!(monotonic_growth(&[1, 1, 2, 3]));
		assert!(!monotonic_growth(&[5, 5, 5, 5]), "flat is steady, not a leak");
		assert!(!monotonic_growth(&[1, 3, 2, 4]), "a fall breaks the growth");
		assert!(!monotonic_growth(&[1, 2]), "too few points to call");
	}

	#[test]
	fn smi_trends_split_by_gpu_and_skip_the_first_window() {
		use crate::gpu_economics::SmiSample;
		let s = |index, mib| SmiSample { index, utilization_pct: 0, memory_used_mib: mib };
		// GPU 0: a start-up jump, then flat. GPU 1: rising throughout.
		let samples: Vec<SmiSample> = (0..10_u64).flat_map(|i| [s(0, if i == 0 { 100 } else { 900 }), s(1, 100 + i)]).collect();
		let trends = smi_trends(&samples);
		assert_eq!(trends.iter().map(|t| (t.gpu_index, t.samples)).collect::<Vec<_>>(), vec![(0, 10), (1, 10)]);
		assert!(!trends[0].growth, "{:?}", trends[0]);
		assert!(trends[1].growth, "{:?}", trends[1]);
	}

	#[test]
	fn old_pool_summaries_still_deserialize() {
		let old = r#"{"max_pool_bytes":1,"rounds":1,"calls":1,"created":1,"created_after_first_round":0,"reused":0,"evicted":0,"peak_idle_bytes_first_round":0,"peak_idle_bytes_later":0,"final_idle_bytes":0,"stable":true}"#;
		let summary: GpuPoolSummary = serde_json::from_str(old).expect("v22 summary");
		assert_eq!(summary.device_memory, None);
	}

	/// Runs on a hardware GPU only (skips otherwise, as splimes' own GPU tests do unless
	/// `WEFT_REQUIRE_GPU` is set). The pool it measures is splimes' one per process, so a GPU
	/// test running beside it (`gpu_economics`' lanes) creates and parks buffer sets inside
	/// its rounds and fails the stability gate for no fault of the pool: `gpu_pool` keeps
	/// them apart.
	#[serial(gpu_pool)]
	#[test]
	fn the_pool_stays_bounded_and_stable_on_a_real_gpu() {
		let profile = GpuMemoryProfile::new("gm-test", GpuMemoryParams { rounds: 3, sizes: vec![1_024, 65_536], knots: 100, ..GpuMemoryParams::default() });
		if let Err(e) = start_gpu(&profile) {
			assert!(std::env::var_os("WEFT_REQUIRE_GPU").is_none(), "WEFT_REQUIRE_GPU is set but: {e}");
			eprintln!("skipping: {e}");
			return;
		}
		let result = run_gpu_memory(&profile).expect("runs");
		assert!(result.correctness.passed(), "{:?} {:?}", result.correctness, result.gpu_pool);
		let pool = result.gpu_pool.expect("summary");
		assert_eq!(pool.calls, 6);
		assert!(pool.stable);
		let device = pool.device_memory.expect("device memory block");
		assert!(!device.allocated_growth, "{device:?}");
		assert!(device.samples == 0 || device.final_reserved_bytes >= device.final_allocated_bytes, "{device:?}");
	}
}
