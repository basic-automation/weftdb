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

use crate::{
	accuracy::AccuracyMetrics, profile::SignalShape, stats::{LatencyCis, LatencyStats}
};

/// Version of the result schema. Bump on any breaking field change.
///
/// v2 added the optional `latency_ci` field (bootstrap confidence intervals).
/// v3 added the `timing` field (end-to-end span breakdown). v4 added the optional
/// `accuracy` field (reconstruction-quality metrics vs a known ground truth). v5
/// added the optional `dataset.signal_shape` field (which synthetic ground-truth
/// curve was generated). All are `#[serde(default)]`, so older artifacts still
/// deserialize.
pub const SCHEMA_VERSION: u32 = 5;

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
	/// Analytic ground-truth signal shape, for a synthetic dataset. Completes the
	/// reproducibility tuple (seed + knobs + shape regenerate the data exactly).
	/// `None` for a line-protocol source (no synthetic shape) and for pre-v5
	/// artifacts; `#[serde(default)]` + `skip_serializing_if` keep those parsing
	/// and omit the key when absent.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub signal_shape: Option<SignalShape>,
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

/// End-to-end wall-clock breakdown of a single `run_profile` invocation, in
/// nanoseconds.
///
/// The latency distribution in [`BenchResult::latency`] measures *only* the
/// adapter call, never the one-time dataset construction — a fair-protocol
/// requirement. This breakdown makes that separation explicit and auditable:
/// `dataset_generation_ns` is the seeded setup cost (excluded from latency),
/// `measured_ns` is the sum of all per-rep adapter calls (the operation under
/// test), and `end_to_end_ns` is the whole timed span. By construction
/// `dataset_generation_ns + measured_ns <= end_to_end_ns`; the remainder
/// ([`Self::overhead_ns`]) is harness bookkeeping (per-rep input clones,
/// summary statistics).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimingBreakdown {
	/// One-time cost of generating the seeded dataset (excluded from latency).
	pub dataset_generation_ns: u64,
	/// Sum of every timed adapter call (the operation under test).
	pub measured_ns: u64,
	/// Whole-run span, from dataset generation through the last rep.
	pub end_to_end_ns: u64,
}

impl TimingBreakdown {
	/// Harness overhead not attributable to dataset generation or the measured
	/// adapter calls (per-rep clones, summary statistics). Saturates at zero so
	/// a zeroed/default breakdown never underflows.
	#[must_use]
	pub const fn overhead_ns(&self) -> u64 {
		self.end_to_end_ns.saturating_sub(self.dataset_generation_ns).saturating_sub(self.measured_ns)
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
	/// Bootstrap confidence intervals for the headline latency statistics.
	/// Optional so v1 artifacts (which predate it) still deserialize and so a
	/// caller may omit CIs for a single-rep run where they are not meaningful.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub latency_ci: Option<LatencyCis>,
	/// Throughput in output points per second, derived from mean latency.
	pub throughput_points_per_sec: f64,
	/// End-to-end timing spans for the run. `#[serde(default)]` so pre-v3
	/// artifacts (which predate it) still deserialize, filling a zeroed
	/// breakdown rather than failing the parse.
	#[serde(default)]
	pub timing: TimingBreakdown,
	/// Correctness verdict; gates whether the latency is publishable.
	pub correctness: CorrectnessReport,
	/// Reconstruction-quality metrics (RMSE / MAE / max-error / bias) scored
	/// against the profile's known analytic ground truth. `Some` only for a
	/// synthetic profile, whose true signal is known; `None` for a line-protocol
	/// source (no ground truth) and for pre-v4 artifacts. `#[serde(default)]` +
	/// `skip_serializing_if` keep old artifacts parsing and omit the key when
	/// absent.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub accuracy: Option<AccuracyMetrics>,
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
		let samples = [100, 200, 300];
		BenchResult { schema_version: SCHEMA_VERSION, profile: "interpolation-heavy-irregular".to_string(), adapter: "dsp".to_string(), workload: "upsample_interpolate".to_string(), reps: 3, dataset: DatasetMeta { input_points: 200, output_points: 1000, irregular: true, missingness_fraction: 0.2, seed: 7, signal_shape: Some(SignalShape::MultiSine) }, latency: LatencyStats::from_samples(&samples), latency_ci: Some(LatencyStats::bootstrap_cis(&samples, &crate::stats::BootstrapConfig::default())), throughput_points_per_sec: 5_000_000.0, timing: TimingBreakdown { dataset_generation_ns: 5_000, measured_ns: 600, end_to_end_ns: 6_200 }, correctness: CorrectnessReport { output_count_ok: true, expected_output_points: 1000, actual_output_points: 1000, values_finite: true }, accuracy: Some(AccuracyMetrics { count: 1000, rmse: 1.5, mae: 1.1, max_abs_error: 4.2, bias: -0.3 }) }
	}

	#[test]
	fn timing_overhead_is_the_saturating_remainder() {
		let t = TimingBreakdown { dataset_generation_ns: 5_000, measured_ns: 600, end_to_end_ns: 6_200 };
		assert_eq!(t.overhead_ns(), 600);
		// A zeroed/inconsistent breakdown must never underflow.
		assert_eq!(TimingBreakdown::default().overhead_ns(), 0);
		assert_eq!(TimingBreakdown { dataset_generation_ns: 10, measured_ns: 10, end_to_end_ns: 5 }.overhead_ns(), 0);
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

	#[test]
	fn v1_artifact_without_latency_ci_still_deserializes() {
		// A pre-v2 artifact has no `latency_ci` key at all; serde's default must
		// fill it with `None` rather than failing the parse.
		let v1 = r#"{
			"schema_version": 1,
			"profile": "interpolation-heavy-irregular",
			"adapter": "dsp",
			"workload": "upsample_interpolate",
			"reps": 3,
			"dataset": { "input_points": 200, "output_points": 1000, "irregular": true, "missingness_fraction": 0.2, "seed": 7 },
			"latency": { "count": 3, "min_ns": 100, "max_ns": 300, "mean_ns": 200, "stddev_ns": 82, "p50_ns": 200, "p95_ns": 300, "p99_ns": 300 },
			"throughput_points_per_sec": 5000000.0,
			"correctness": { "output_count_ok": true, "expected_output_points": 1000, "actual_output_points": 1000, "values_finite": true }
		}"#;
		let parsed: BenchResult = serde_json::from_str(v1).expect("v1 artifact must still parse");
		assert!(parsed.latency_ci.is_none());
		assert_eq!(parsed.timing, TimingBreakdown::default());
		assert_eq!(parsed.schema_version, 1);
		assert!(parsed.is_publishable());
	}

	#[test]
	fn v2_artifact_without_timing_still_deserializes() {
		// A v2 artifact carries `latency_ci` but no `timing` key; serde's default
		// must fill a zeroed breakdown rather than failing the parse.
		let v2 = r#"{
			"schema_version": 2,
			"profile": "interpolation-heavy-irregular",
			"adapter": "dsp",
			"workload": "upsample_interpolate",
			"reps": 3,
			"dataset": { "input_points": 200, "output_points": 1000, "irregular": true, "missingness_fraction": 0.2, "seed": 7 },
			"latency": { "count": 3, "min_ns": 100, "max_ns": 300, "mean_ns": 200, "stddev_ns": 82, "p50_ns": 200, "p95_ns": 300, "p99_ns": 300 },
			"throughput_points_per_sec": 5000000.0,
			"correctness": { "output_count_ok": true, "expected_output_points": 1000, "actual_output_points": 1000, "values_finite": true }
		}"#;
		let parsed: BenchResult = serde_json::from_str(v2).expect("v2 artifact must still parse");
		assert_eq!(parsed.timing, TimingBreakdown::default());
		assert!(parsed.accuracy.is_none(), "a pre-v4 artifact carries no accuracy");
		assert_eq!(parsed.schema_version, 2);
		assert!(parsed.is_publishable());
	}

	#[test]
	fn v4_artifact_without_signal_shape_still_deserializes() {
		// A v4 artifact carries accuracy but no `dataset.signal_shape`; serde's
		// default must fill `None` rather than failing the parse.
		let v4 = r#"{
			"schema_version": 4,
			"profile": "interpolation-heavy-irregular",
			"adapter": "dsp",
			"workload": "upsample_interpolate",
			"reps": 3,
			"dataset": { "input_points": 200, "output_points": 1000, "irregular": true, "missingness_fraction": 0.2, "seed": 7 },
			"latency": { "count": 3, "min_ns": 100, "max_ns": 300, "mean_ns": 200, "stddev_ns": 82, "p50_ns": 200, "p95_ns": 300, "p99_ns": 300 },
			"throughput_points_per_sec": 5000000.0,
			"correctness": { "output_count_ok": true, "expected_output_points": 1000, "actual_output_points": 1000, "values_finite": true }
		}"#;
		let parsed: BenchResult = serde_json::from_str(v4).expect("v4 artifact must still parse");
		assert!(parsed.dataset.signal_shape.is_none(), "a pre-v5 artifact carries no signal shape");
		assert!(parsed.is_publishable());
	}

	#[test]
	fn signal_shape_round_trips_and_serializes_lowercase() {
		// The recorded shape must survive a JSON round-trip and serialize as its
		// stable lowercase token (matching the CLI), not the variant name.
		let result = sample_result();
		let json = serde_json::to_string(&result).expect("serialize");
		assert!(json.contains("\"signal_shape\":\"multisine\""), "shape must serialize lowercase, got: {json}");
		let back: BenchResult = serde_json::from_str(&json).expect("deserialize");
		assert_eq!(back.dataset.signal_shape, Some(SignalShape::MultiSine));
	}

	#[test]
	fn signal_shape_is_omitted_from_json_when_absent() {
		// A line-protocol-sourced dataset has no synthetic shape; the key must be
		// omitted entirely rather than emitted as `null`.
		let mut result = sample_result();
		result.dataset.signal_shape = None;
		let json = serde_json::to_string(&result).expect("serialize");
		assert!(!json.contains("signal_shape"), "absent shape must be omitted, got: {json}");
	}

	#[test]
	fn accuracy_is_omitted_from_json_when_absent() {
		// A line-protocol-sourced result has no ground truth, so `accuracy` is None;
		// `skip_serializing_if` must keep the key out of the artifact entirely (not
		// emit `"accuracy": null`), and it must round-trip back to None.
		let mut result = sample_result();
		result.accuracy = None;
		let json = serde_json::to_string(&result).expect("serialize");
		assert!(!json.contains("accuracy"), "absent accuracy must be omitted, got: {json}");
		let back: BenchResult = serde_json::from_str(&json).expect("deserialize");
		assert!(back.accuracy.is_none());
	}
}
