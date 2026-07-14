//! Downsample workload — the aggregation query as a benchmark.
//!
//! Reducing a series into grid-aligned buckets (min/max/avg/sum/first/last) is the
//! classic TSDB aggregation workload the roadmap lists for Phase 1. This drives DSP's
//! canonical reduction — the vendor-neutral [`dsp_reduce`] crate the HTTP
//! `downsample` endpoint shares — over a seeded dense series, timing the reduction
//! and reporting throughput (points reduced per second).
//!
//! Unlike the storage workloads this operates on an in-memory `splimes::Point`
//! series (no sealed segment): the corpus is a dense, regularly-spaced signal so each
//! bucket holds many samples, and the workload asks [`dsp_reduce::reduce`] to collapse
//! it to a coarser grid. Correctness is gated on the reduction being total — every
//! in-range input point lands in exactly one bucket (the bucket counts sum to the
//! input size) — and every reduced value being finite. The [`BenchResult`] carries
//! the `downsample` workload label, the reduction-latency distribution, and
//! throughput; it has no accuracy (a reduction has no analytic ground truth beyond
//! the exact aggregate, which the correctness gate covers).

use std::time::Instant;

use bigdecimal::{BigDecimal, FromPrimitive, ToPrimitive};
use chrono::{DateTime, Duration, TimeZone, Utc};
pub use dsp_reduce::Aggregation;
use dsp_reduce::{reduce, Bucket};
use rand::{Rng, SeedableRng};
use rand_chacha::ChaCha8Rng;
use splimes::{Point, Resolution};

use crate::{
	schema::{BenchResult, CorrectnessReport, DatasetMeta, TimingBreakdown, SCHEMA_VERSION}, stats::{BootstrapConfig, LatencyStats}
};

/// Workload class label recorded for the downsample workload.
const WORKLOAD_DOWNSAMPLE: &str = "downsample";

/// A fixed epoch anchor (2020-01-01T00:00:00Z) so a generated corpus is stable across
/// machines and runs, as the reproducibility rules require.
const EPOCH_ANCHOR_SECS: i64 = 1_577_836_800;

/// Default seed for downsample profiles.
pub const DEFAULT_DOWNSAMPLE_SEED: u64 = 0x00DB_5EED_D065;

/// The tunable knobs for a downsample profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownsampleParams {
	/// RNG seed; publishing it regenerates the corpus exactly.
	pub seed: u64,
	/// Number of input samples (spaced [`Self::input_stride_secs`] apart).
	pub point_count: usize,
	/// Spacing between input samples, in seconds.
	pub input_stride_secs: i64,
	/// Bucket resolution the series is downsampled to.
	pub bucket_resolution: Resolution,
	/// Reductions computed per bucket (empty ⇒ [`Aggregation::DEFAULT`]).
	pub aggregations: Vec<Aggregation>,
}

impl Default for DownsampleParams {
	/// The flagship downsample knob set: 60 000 samples at 1-second spacing (~16.7
	/// hours) reduced to per-minute buckets (~60 samples/bucket), every reduction.
	fn default() -> Self {
		Self { seed: DEFAULT_DOWNSAMPLE_SEED, point_count: 60_000, input_stride_secs: 1, bucket_resolution: Resolution::Minutes, aggregations: Aggregation::ALL.to_vec() }
	}
}

/// A reproducible downsample workload profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownsampleProfile {
	/// Human-readable profile name, recorded in results.
	pub name: String,
	/// RNG seed.
	pub seed: u64,
	/// Number of input samples.
	pub point_count: usize,
	/// Spacing between input samples, in seconds.
	pub input_stride_secs: i64,
	/// Bucket resolution.
	pub bucket_resolution: Resolution,
	/// Reductions computed per bucket.
	pub aggregations: Vec<Aggregation>,
}

impl DownsampleProfile {
	/// The flagship per-minute downsample profile.
	#[must_use]
	pub fn per_minute(name: impl Into<String>) -> Self {
		Self::new(name, DownsampleParams::default())
	}

	/// Build a downsample profile from an explicit [`DownsampleParams`] knob set.
	#[must_use]
	pub fn new(name: impl Into<String>, params: DownsampleParams) -> Self {
		let DownsampleParams { seed, point_count, input_stride_secs, bucket_resolution, aggregations } = params;
		Self { name: name.into(), seed, point_count: point_count.max(2), input_stride_secs: input_stride_secs.max(1), bucket_resolution, aggregations }
	}

	/// The fixed epoch anchor (2020-01-01T00:00:00Z).
	fn anchor() -> DateTime<Utc> {
		Utc.timestamp_opt(EPOCH_ANCHOR_SECS, 0).single().expect("valid fixed epoch anchor")
	}

	/// Generate the seeded, regularly-spaced input series (a smooth sinusoid plus a
	/// little seeded noise, so per-bucket min/max/avg differ meaningfully).
	#[must_use]
	pub fn generate(&self) -> Vec<Point> {
		use std::f64::consts::TAU;
		let mut rng = ChaCha8Rng::seed_from_u64(self.seed);
		let anchor = Self::anchor();
		let n = self.point_count.max(2);
		(0..n).map(|i| {
			#[allow(clippy::cast_possible_wrap)]
			let timestamp = anchor + Duration::seconds(i as i64 * self.input_stride_secs);
			#[allow(clippy::cast_precision_loss)]
			let phase = i as f64 / 500.0;
			let noise = rng.random_range(-1.0..=1.0);
			let signal = 20.0_f64.mul_add((phase * TAU).sin(), 50.0) + noise;
			Point { timestamp, value: BigDecimal::from_f64(signal).unwrap_or_else(|| BigDecimal::from(50)) }
		}).collect()
	}
}

/// Elapsed nanoseconds since `since`, saturated into a `u64`.
#[allow(clippy::cast_possible_truncation)]
fn span_ns(since: Instant) -> u64 {
	since.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
}

/// Run a downsample profile for `reps` timed repetitions and return a
/// fully-populated [`BenchResult`] (workload `downsample`).
///
/// The corpus is generated **once** (setup, excluded from latency); each rep
/// re-reduces the whole series via [`dsp_reduce::reduce`] and the timed span covers
/// only that. Correctness is gated on the reduction being total — the bucket counts
/// sum to the input size — and every reduced value being finite. Throughput is input
/// points reduced per second.
///
/// # Errors
///
/// Returns an error if `reps == 0` or if a reduction fails (a timestamp that cannot
/// be indexed at the resolution).
pub fn run_downsample(profile: &DownsampleProfile, reps: usize) -> anyhow::Result<BenchResult> {
	anyhow::ensure!(reps > 0, "reps must be > 0");

	let run_start = Instant::now();

	let setup_start = Instant::now();
	let points = profile.generate();
	let setup_ns = span_ns(setup_start);

	let mut samples_ns: Vec<u64> = Vec::with_capacity(reps);
	let mut last_buckets: Vec<Bucket> = Vec::new();
	for _ in 0..reps {
		let t0 = Instant::now();
		let buckets = reduce(&points, profile.bucket_resolution, None, None, &profile.aggregations).map_err(|e| anyhow::anyhow!("downsample reduction failed: {e}"))?;
		samples_ns.push(span_ns(t0));
		last_buckets = buckets;
	}

	let measured_ns = samples_ns.iter().copied().fold(0_u64, u64::saturating_add);
	let end_to_end_ns = span_ns(run_start);
	let timing = TimingBreakdown { dataset_generation_ns: setup_ns, measured_ns, end_to_end_ns };

	// Correctness: the reduction is total (every input point lands in exactly one
	// bucket, so the counts sum to the input size), the buckets are time-ordered, and
	// every reduced value is finite.
	let total_count: usize = last_buckets.iter().map(|b| b.count).sum();
	let ordered = last_buckets.windows(2).all(|w| w[0].timestamp < w[1].timestamp);
	let values_finite = last_buckets.iter().flat_map(|b| b.values.values()).all(|v| v.to_f64().is_some_and(f64::is_finite));
	let correctness = CorrectnessReport { output_count_ok: total_count == points.len() && ordered && !last_buckets.is_empty(), expected_output_points: points.len(), actual_output_points: total_count, values_finite };

	let latency = LatencyStats::from_samples(&samples_ns);
	let latency_ci = Some(LatencyStats::bootstrap_cis(&samples_ns, &BootstrapConfig { seed: profile.seed ^ 0x_C0FF_EE15_C0DE_u64, ..BootstrapConfig::default() }));
	// Throughput is input points reduced per second.
	let throughput_points_per_sec = if latency.mean_ns > 0 {
		#[allow(clippy::cast_precision_loss)]
		let secs = latency.mean_ns as f64 / 1e9;
		#[allow(clippy::cast_precision_loss)]
		let points_f = points.len() as f64;
		points_f / secs
	} else {
		0.0
	};

	Ok(BenchResult {
		schema_version: SCHEMA_VERSION, profile: profile.name.clone(), adapter: "dsp".to_string(), workload: WORKLOAD_DOWNSAMPLE.to_string(), reps, dataset: DatasetMeta { input_points: points.len(), output_points: last_buckets.len(), irregular: false, missingness_fraction: 0.0, seed: profile.seed, signal_shape: None }, latency, latency_ci, throughput_points_per_sec, timing, correctness, accuracy: None, storage: None
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	fn small() -> DownsampleProfile {
		DownsampleProfile::new("ds-small", DownsampleParams { point_count: 3_600, input_stride_secs: 1, bucket_resolution: Resolution::Minutes, ..DownsampleParams::default() })
	}

	#[test]
	fn generation_is_deterministic_for_a_seed() {
		let profile = small();
		assert_eq!(profile.generate(), profile.generate(), "same seed -> identical corpus");
	}

	#[test]
	fn per_minute_run_reduces_totally_and_passes_correctness() {
		let profile = small();
		let result = run_downsample(&profile, 3).expect("downsample run succeeds");
		assert_eq!(result.adapter, "dsp");
		assert_eq!(result.workload, "downsample");
		assert_eq!(result.reps, 3);
		assert_eq!(result.latency.count, 3);
		assert_eq!(result.dataset.input_points, 3_600);
		// 3600 one-second samples at per-minute buckets => 60 buckets.
		assert_eq!(result.dataset.output_points, 60, "3600s at per-minute buckets is 60 buckets");
		assert!(result.accuracy.is_none());
		assert!(result.storage.is_none(), "downsample operates on a Point series, not a sealed segment");
		assert!(result.correctness.passed(), "the reduction must be total: {:?}", result.correctness);
		// The reduction is total: the bucket counts sum to the input size.
		assert_eq!(result.correctness.actual_output_points, 3_600);
		assert!(result.is_publishable());
		assert!(result.throughput_points_per_sec > 0.0);
		assert!(result.timing.measured_ns > 0);
	}

	#[test]
	fn hourly_buckets_collapse_further() {
		let profile = DownsampleProfile::new("ds-hourly", DownsampleParams { point_count: 7_200, input_stride_secs: 1, bucket_resolution: Resolution::Hours, ..DownsampleParams::default() });
		let result = run_downsample(&profile, 2).expect("hourly run succeeds");
		// 7200 one-second samples => 2 hours => 2 buckets.
		assert_eq!(result.dataset.output_points, 2, "7200s at hourly buckets is 2 buckets");
		assert!(result.correctness.passed());
		assert_eq!(result.correctness.actual_output_points, 7_200, "still total");
	}

	#[test]
	fn a_subset_of_aggregations_is_honored() {
		let profile = DownsampleProfile::new("ds-sum", DownsampleParams { point_count: 600, input_stride_secs: 1, bucket_resolution: Resolution::Minutes, aggregations: vec![Aggregation::Sum], ..DownsampleParams::default() });
		let result = run_downsample(&profile, 2).expect("run succeeds");
		assert!(result.correctness.passed());
		assert_eq!(result.dataset.output_points, 10, "600s at per-minute buckets is 10 buckets");
	}

	#[test]
	fn zero_reps_is_rejected() {
		assert!(run_downsample(&small(), 0).is_err(), "zero reps must be rejected");
	}

	#[test]
	fn result_round_trips_through_json() {
		let result = run_downsample(&small(), 3).expect("run succeeds");
		let json = serde_json::to_string(&result).expect("serialize");
		let back: BenchResult = serde_json::from_str(&json).expect("deserialize");
		assert_eq!(result.workload, back.workload);
		assert_eq!(result.dataset, back.dataset);
		assert_eq!(result.correctness, back.correctness);
		assert!(back.accuracy.is_none());
		assert!(back.storage.is_none());
	}
}
