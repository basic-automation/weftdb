//! Result schema for DSP-Bench runs.
//!
//! These types are the serializable record of a single benchmark run. They are
//! intentionally adapter- and workload-agnostic so that every system adapter
//! (DSP today; `ClickHouse` / `InfluxDB 3` / `QuestDB` / `TimescaleDB` /
//! `DuckDB` later)
//! emits the same shape. The roadmap's benchmark-report template and
//! anti-Goodhart policy require that every number ship with the context needed
//! to reproduce and trust it — hence the explicit `dataset`, `correctness`, and
//! `reps` fields alongside the latency distribution.
//!
//! `SCHEMA_VERSION` is bumped whenever the on-disk JSON shape changes so old
//! result artifacts remain interpretable.

use serde::{Deserialize, Serialize};

use crate::stats::LatencyStats;

/// Version of the result schema. Bump on any breaking field change.
pub const SCHEMA_VERSION: u32 = 1;

/// Metadata describing the dataset a result was measured against.
///
/// Captures the knobs that determine how interpolation-heavy and how irregular
/// the workload was, plus the RNG seed so the exact dataset can be regenerated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DatasetMeta {
	/// Number of input measurements fed to the adapter.
	pub input_points: usize,
	/// Number of output points produced (interpolated grid size).
	pub output_points: usize,
	/// Whether input timestamps are irregularly spaced.
	pub irregular: bool,
	/// Fraction of the regular grid removed to create gaps (`0.0..=1.0`).
	pub missingness_fraction: f64,
	/// Seed used to generate the dataset (publishable for reproducibility).
	pub seed: u64,
}

/// Correctness verdict for a run. A number is only publishable when correctness
/// passes — this mirrors the roadmap's "correctness gates every performance
/// number" rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorrectnessReport {
	/// True when the adapter produced a non-empty output of the expected size.
	pub output_count_ok: bool,
	/// Output grid size the harness expected for this range/resolution.
	pub expected_output_points: usize,
	/// Output grid size the adapter actually produced.
	pub actual_output_points: usize,
	/// True when every produced value is a finite, well-formed number.
	pub values_finite: bool,
}

impl CorrectnessReport {
	/// Overall pass/fail: counts line up and all values are finite.
	#[must_use]
	pub const fn passed(&self) -> bool {
		self.output_count_ok && self.values_finite
	}
}

/// A single DSP-Bench result: one workload, one adapter, one profile, N reps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchResult {
	/// Schema version this record was written with.
	pub schema_version: u32,
	/// Workload profile name (e.g. `interpolation-heavy-irregular`).
	pub profile: String,
	/// System adapter name (e.g. `dsp`).
	pub adapter: String,
	/// Workload class (e.g. `upsample_interpolate`).
	pub workload: String,
	/// Number of timed repetitions.
	pub reps: usize,
	/// Dataset characteristics + seed.
	pub dataset: DatasetMeta,
	/// Latency distribution across the timed reps.
	pub latency: LatencyStats,
	/// Throughput in output points per second, derived from mean latency.
	pub throughput_points_per_sec: f64,
	/// Correctness verdict; gates whether the latency is publishable.
	pub correctness: CorrectnessReport,
}

impl BenchResult {
	/// True when this result is fit to publish: correctness passed and at least
	/// one rep was timed.
	#[must_use]
	pub const fn is_publishable(&self) -> bool {
		self.correctness.passed() && self.reps > 0
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn sample_result() -> BenchResult {
		BenchResult { schema_version: SCHEMA_VERSION, profile: "interpolation-heavy-irregular".to_string(), adapter: "dsp".to_string(), workload: "upsample_interpolate".to_string(), reps: 3, dataset: DatasetMeta { input_points: 200, output_points: 1000, irregular: true, missingness_fraction: 0.2, seed: 7 }, latency: LatencyStats::from_samples(&[100, 200, 300]), throughput_points_per_sec: 5_000_000.0, correctness: CorrectnessReport { output_count_ok: true, expected_output_points: 1000, actual_output_points: 1000, values_finite: true } }
	}

	#[test]
	fn result_round_trips_through_json() {
		let result = sample_result();
		let json = serde_json::to_string(&result).expect("serialize");
		let back: BenchResult = serde_json::from_str(&json).expect("deserialize");
		assert_eq!(result, back);
	}

	#[test]
	fn publishable_requires_correctness_and_reps() {
		let mut result = sample_result();
		assert!(result.is_publishable());

		result.correctness.values_finite = false;
		assert!(!result.is_publishable());

		result.correctness.values_finite = true;
		result.reps = 0;
		assert!(!result.is_publishable());
	}
}
