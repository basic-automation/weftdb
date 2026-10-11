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
//! behaviour. It needs a hardware GPU: a CPU/software adapter is refused, as the server does
//! not calibrate onto one either.

use std::time::Instant;

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
}

impl Default for GpuMemoryParams {
	/// Twenty rounds of 16 Ki-, 1 Mi- and 4 Mi-point calls over 1,000 knots, default pool.
	fn default() -> Self {
		Self { seed: DEFAULT_GPU_MEMORY_SEED, rounds: 20, sizes: vec![16 << 10, 1 << 20, 4 << 20], knots: 1_000, pool_mib: None }
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
		Self { name: name.into(), params: GpuMemoryParams { rounds: params.rounds.max(1), knots: params.knots.max(2), sizes, ..params } }
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
	#[allow(clippy::cast_precision_loss)]
	let values = offsets.iter().map(|&o| 20.0_f64.mul_add((o as f64 / 600.0).sin(), 50.0) + rng.random_range(-1.0..=1.0)).collect();
	(offsets.into_iter().map(|o| anchor() + Duration::seconds(o)).collect(), values)
}

/// Elapsed nanoseconds since `since`, saturated into a `u64`.
#[allow(clippy::cast_possible_truncation)]
fn span_ns(since: Instant) -> u64 {
	since.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
}

/// Start the GPU (with the profile's pool cap) and describe it for the report.
///
/// # Errors
///
/// If the cap cannot be applied (the GPU already started with another), no GPU starts, or
/// the adapter is a CPU/software one.
pub fn start_gpu(profile: &GpuMemoryProfile) -> anyhow::Result<EngineMetadata> {
	if let Some(mib) = profile.params.pool_mib {
		splimes::configure_gpu(GpuConfig::DEFAULT.with_max_pool_bytes(mib << 20)).map_err(|e| anyhow::anyhow!("cannot set the GPU pool cap: {e}"))?;
	}
	let info = splimes::prewarm_gpu().map_err(|e| anyhow::anyhow!("the gpu-memory workload needs a GPU: {e}"))?;
	anyhow::ensure!(!is_software_adapter(&info.device_type, &info.name), "the gpu-memory workload needs a hardware GPU; {} is a CPU/software adapter", info.name);
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
	let (mut samples_ns, mut complete, mut finite) = (Vec::new(), true, true);
	let (mut peak_first, mut peak_later, mut created_after_first) = (0_u64, 0_u64, 0_u64);
	let mut output_points = 0_usize;
	let mut order: Vec<usize> = (0..inputs.len()).collect();
	for round in 0..p.rounds {
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
			if round == 0 {
				peak_first = peak_first.max(idle);
			} else {
				peak_later = peak_later.max(idle);
			}
		}
		if round > 0 {
			created_after_first += stats()?.created - created_before_round;
		}
	}
	let after = stats()?;
	let measured_ns = samples_ns.iter().copied().fold(0_u64, u64::saturating_add);
	let timing = TimingBreakdown { dataset_generation_ns: generation_ns, measured_ns, end_to_end_ns: span_ns(run_start) };
	let stable = peak_later <= peak_first && peak_first.max(peak_later) <= max_pool_bytes;
	let summary = GpuPoolSummary { max_pool_bytes, rounds: p.rounds, calls: samples_ns.len(), created: after.created - before.created, created_after_first_round: created_after_first, reused: after.reused - before.reused, evicted: after.evicted - before.evicted, peak_idle_bytes_first_round: peak_first, peak_idle_bytes_later: peak_later, final_idle_bytes: after.idle_bytes, stable };

	let correctness = CorrectnessReport { output_count_ok: complete && stable, expected_output_points: p.sizes.iter().sum::<usize>() * p.rounds, actual_output_points: output_points, values_finite: finite };
	let latency = LatencyStats::from_samples(&samples_ns);
	let latency_ci = Some(LatencyStats::bootstrap_cis(&samples_ns, &BootstrapConfig { seed: p.seed ^ 0x06B0_C0DE_u64, ..BootstrapConfig::default() }));
	#[allow(clippy::cast_precision_loss)]
	let throughput_points_per_sec = if measured_ns > 0 { output_points as f64 / (measured_ns as f64 / 1e9) } else { 0.0 };
	Ok(BenchResult { schema_version: SCHEMA_VERSION, profile: profile.name.clone(), adapter: "weftdb".to_string(), workload: WORKLOAD_GPU_MEMORY.to_string(), reps: p.rounds, dataset: DatasetMeta { input_points: inputs.iter().map(|(_, (t, _))| t.len()).sum(), output_points, irregular: true, missingness_fraction: 0.0, seed: p.seed, signal_shape: None }, latency, latency_ci, throughput_points_per_sec, timing, correctness, accuracy: None, storage: None, cold_warm: ColdWarm::from_samples(&samples_ns), gpu_pool: Some(summary) })
}

#[cfg(test)]
mod tests {
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

	/// Runs on a hardware GPU only (skips otherwise, as splimes' own GPU tests do unless
	/// `WEFT_REQUIRE_GPU` is set).
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
	}
}
