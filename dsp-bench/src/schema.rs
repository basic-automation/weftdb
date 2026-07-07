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

use bigdecimal::BigDecimal;
use dsp_physical_type::{encode_delta_of_delta, recommend_encoding, TimeUnit};
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
/// curve was generated). v6 added the optional `storage` field (north-star
/// bytes/point under the fastest-safe physical encoding). v7 added
/// `storage.realized_value_bytes` (the exact on-disk value payload, below the naive
/// `estimated_value_bytes` for varint-coded encodings). v8 flipped
/// `storage.realized_value_bytes` to the codec the segment actually selects (the
/// smaller of the per-value varint and the fixed-width bit-pack for a `ScaledI64`
/// column) and added `storage.value_codec` (which value codec that was). All are
/// `#[serde(default)]`, so older artifacts still deserialize.
pub const SCHEMA_VERSION: u32 = 8;

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

/// Storage-cost estimate for a result's stored columns — value **and**
/// timestamp — under the fastest-safe encodings.
///
/// DSP's north star is *dollars per billion interpolated points at a p95 target*;
/// storage **bytes per point** is the cost term that rests on, and a stored point
/// is a `(timestamp, value)` pair, so an honest figure must account for both
/// columns. This block turns that into a measured benchmark outcome:
///
/// - the **value** column is run through the vendor-neutral `dsp-physical-type`
///   advisory selector ([`recommend_encoding`]) — which encoding was chosen, its
///   byte footprint, the realized bytes/point, and the exactness achieved within
///   the requested bound (per hard constraint #4 any loss is reported via
///   `lossy_count` / `max_abs_error`, never silent);
/// - the **timestamp** column is delta-of-delta encoded
///   ([`encode_delta_of_delta`]) then packed with the cheapest of zig-zag-varint,
///   RLE, fixed-width bit-packing, or the Gorilla variable-length codec — lossless,
///   and near-free for a regular series;
/// - [`total_bytes_per_point`](Self::total_bytes_per_point) sums the two, the
///   headline north-star figure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StorageEstimate {
	/// Name of the value physical encoding selected (e.g. `scaled_i64`, `f64`).
	pub physical_type: String,
	/// Number of values in the stored column.
	pub value_count: usize,
	/// Estimated byte footprint of the value column under that encoding (the naive
	/// fixed-width `count * width` figure).
	pub estimated_value_bytes: usize,
	/// **Realized** on-disk byte footprint of the value column — the exact payload a
	/// sealed `.dspseg` frame writes under the codec it actually selects (matches
	/// [`ColumnEncoding::best_serialized_bytes`](dsp_physical_type::ColumnEncoding::best_serialized_bytes)).
	/// For a `scaled_i64` column this is the smaller of the per-value varint and the
	/// fixed-width bit-pack (see [`value_codec`](Self::value_codec)), always below
	/// [`estimated_value_bytes`](Self::estimated_value_bytes) — the accurate bytes/point
	/// the naive estimate over-reports. Defaults to `0` on a pre-v7 artifact.
	#[serde(default)]
	pub realized_value_bytes: usize,
	/// Which value codec [`realized_value_bytes`](Self::realized_value_bytes) reflects:
	/// `varint` (the general per-value payload) or `scaled_bitpack` (fixed-width
	/// bit-packing of a `ScaledI64` column's mantissas, chosen when strictly smaller).
	/// Empty on a pre-v8 artifact.
	#[serde(default)]
	pub value_codec: String,
	/// Realized value-column storage cost: `estimated_value_bytes / value_count`
	/// (0 for an empty column).
	pub bytes_per_point: f64,
	/// Whether every value reconstructs exactly under the chosen encoding.
	pub is_exact: bool,
	/// How many values encoded lossily (0 ⇒ the column is exact).
	pub lossy_count: usize,
	/// Worst per-value absolute reconstruction error, as a decimal string (`"0"`
	/// when exact). A string keeps arbitrary precision out of the `f64` trap.
	pub max_abs_error: String,
	/// Error tolerance the encoding was selected against, as a decimal string.
	pub tolerance: String,
	/// Epoch unit the timestamp column was encoded in (e.g. `micros`).
	#[serde(default)]
	pub timestamp_unit: String,
	/// Timestamp-column encoding chosen: `delta_of_delta`, `delta_of_delta_rle`
	/// when run-length coding the second differences is cheaper (long identical
	/// runs), `delta_of_delta_bitpack` when fixed-width bit-packing is cheapest
	/// (a regular or small-jitter series), or `delta_of_delta_gorilla` when the
	/// Gorilla variable-length codec wins (scattered single jitter, where RLE cannot
	/// form runs).
	#[serde(default)]
	pub timestamp_encoding: String,
	/// Estimated byte footprint of the timestamp column (anchor + varint stream),
	/// 0 when no timestamps were supplied.
	#[serde(default)]
	pub timestamp_bytes: usize,
	/// Realized timestamp-column storage cost in bytes/point (0 for an empty
	/// column).
	#[serde(default)]
	pub timestamp_bytes_per_point: f64,
	/// The headline north-star figure: value + timestamp bytes/point.
	#[serde(default)]
	pub total_bytes_per_point: f64,
}

impl StorageEstimate {
	/// Estimate the stored value-column cost of `values` only (no timestamp
	/// column), under the narrowest hot-path encoding within `tolerance`.
	///
	/// A `tolerance` of zero selects the narrowest *lossless* encoding. The
	/// timestamp fields are left empty / zero and `total_bytes_per_point` equals
	/// the value `bytes_per_point`; use [`from_columns`](Self::from_columns) for a
	/// complete `(timestamp, value)` figure.
	#[must_use]
	pub fn from_values(values: &[BigDecimal], tolerance: &BigDecimal) -> Self {
		Self::from_columns(values, &[], TimeUnit::Micros, tolerance)
	}

	/// Estimate the full stored-point cost: the value column under the
	/// fastest-safe encoding plus the `timestamps` column under lossless
	/// delta-of-delta + varint coding.
	///
	/// `timestamps` are integer epochs in `unit`. An empty `timestamps` slice
	/// contributes zero timestamp bytes (the value-only case).
	#[must_use]
	pub fn from_columns(values: &[BigDecimal], timestamps: &[i64], unit: TimeUnit, tolerance: &BigDecimal) -> Self {
		let enc = recommend_encoding(values, tolerance);
		let estimated_value_bytes = enc.estimated_bytes();
		let realized_value_bytes = enc.best_serialized_bytes();
		let value_codec = enc.best_value_codec().to_string();
		let value_count = enc.len();
		let bytes_per_point = per_point(estimated_value_bytes, value_count);

		// Encode the timestamp column delta-of-delta, then let `dsp-physical-type`
		// pick the cheapest of plain varint, RLE-of-second-differences, fixed-width
		// bit-packing, or the Gorilla variable-length codec (each wins a different
		// regime: RLE on regular runs, bit-pack on small jitter, Gorilla on scattered
		// single jitter) — the same single source of truth a stored `Segment` uses,
		// so the advisory bench estimate and a realized segment never disagree on
		// size or codec label.
		let (timestamp_bytes, timestamp_encoding) = if timestamps.is_empty() {
			(0, "delta_of_delta")
		} else {
			let dod = encode_delta_of_delta(timestamps, unit);
			(dod.best_estimated_bytes(), dod.best_encoding_name())
		};
		let timestamp_bytes_per_point = per_point(timestamp_bytes, timestamps.len());

		Self { physical_type: enc.physical_type.name().to_string(), value_count, estimated_value_bytes, realized_value_bytes, value_codec, bytes_per_point, is_exact: enc.is_exact(), lossy_count: enc.lossy_count, max_abs_error: enc.max_abs_error.to_string(), tolerance: tolerance.to_string(), timestamp_unit: unit.name().to_string(), timestamp_encoding: timestamp_encoding.to_string(), timestamp_bytes, timestamp_bytes_per_point, total_bytes_per_point: bytes_per_point + timestamp_bytes_per_point }
	}
}

/// `bytes / count` as a rate, or `0.0` for an empty column.
#[must_use]
fn per_point(bytes: usize, count: usize) -> f64 {
	if count == 0 {
		return 0.0;
	}
	#[allow(clippy::cast_precision_loss)]
	let (bytes, count) = (bytes as f64, count as f64);
	bytes / count
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
	/// Storage-cost estimate for the stored value column (north-star
	/// bytes/point). `Some` once computed by `run_profile`; `None` for pre-v6
	/// artifacts and any caller that omits it. `#[serde(default)]` +
	/// `skip_serializing_if` keep old artifacts parsing and omit the key when
	/// absent.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub storage: Option<StorageEstimate>,
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
		BenchResult { schema_version: SCHEMA_VERSION, profile: "interpolation-heavy-irregular".to_string(), adapter: "dsp".to_string(), workload: "upsample_interpolate".to_string(), reps: 3, dataset: DatasetMeta { input_points: 200, output_points: 1000, irregular: true, missingness_fraction: 0.2, seed: 7, signal_shape: Some(SignalShape::MultiSine) }, latency: LatencyStats::from_samples(&samples), latency_ci: Some(LatencyStats::bootstrap_cis(&samples, &crate::stats::BootstrapConfig::default())), throughput_points_per_sec: 5_000_000.0, timing: TimingBreakdown { dataset_generation_ns: 5_000, measured_ns: 600, end_to_end_ns: 6_200 }, correctness: CorrectnessReport { output_count_ok: true, expected_output_points: 1000, actual_output_points: 1000, values_finite: true }, accuracy: Some(AccuracyMetrics { count: 1000, rmse: 1.5, mae: 1.1, max_abs_error: 4.2, bias: -0.3 }), storage: Some(StorageEstimate { physical_type: "scaled_i64".to_string(), value_count: 200, estimated_value_bytes: 1600, realized_value_bytes: 420, value_codec: "varint".to_string(), bytes_per_point: 8.0, is_exact: true, lossy_count: 0, max_abs_error: "0".to_string(), tolerance: "0".to_string(), timestamp_unit: "micros".to_string(), timestamp_encoding: "delta_of_delta".to_string(), timestamp_bytes: 208, timestamp_bytes_per_point: 1.04, total_bytes_per_point: 9.04 }) }
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
	fn v5_artifact_without_storage_still_deserializes() {
		// A v5 artifact carries signal_shape + accuracy but no `storage` key; serde's
		// default must fill `None` rather than failing the parse.
		let v5 = r#"{
			"schema_version": 5,
			"profile": "interpolation-heavy-irregular",
			"adapter": "dsp",
			"workload": "upsample_interpolate",
			"reps": 3,
			"dataset": { "input_points": 200, "output_points": 1000, "irregular": true, "missingness_fraction": 0.2, "seed": 7, "signal_shape": "multisine" },
			"latency": { "count": 3, "min_ns": 100, "max_ns": 300, "mean_ns": 200, "stddev_ns": 82, "p50_ns": 200, "p95_ns": 300, "p99_ns": 300 },
			"throughput_points_per_sec": 5000000.0,
			"correctness": { "output_count_ok": true, "expected_output_points": 1000, "actual_output_points": 1000, "values_finite": true }
		}"#;
		let parsed: BenchResult = serde_json::from_str(v5).expect("v5 artifact must still parse");
		assert!(parsed.storage.is_none(), "a pre-v6 artifact carries no storage estimate");
		assert!(parsed.is_publishable());
	}

	#[test]
	fn storage_is_omitted_from_json_when_absent() {
		// A caller may omit the storage estimate; `skip_serializing_if` must keep the
		// key out of the artifact entirely rather than emit `"storage": null`.
		let mut result = sample_result();
		result.storage = None;
		let json = serde_json::to_string(&result).expect("serialize");
		assert!(!json.contains("storage"), "absent storage must be omitted, got: {json}");
		let back: BenchResult = serde_json::from_str(&json).expect("deserialize");
		assert!(back.storage.is_none());
	}

	#[test]
	fn storage_estimate_lossless_scaled_int_for_decimal_tenths() {
		// 0.1 / 0.2 / 0.3 are *not* exactly representable in binary float, so with
		// tolerance 0 the advisor skips F32/F64 and picks an exact 8-byte ScaledI64
		// (scale 1) — a genuine lossless bytes/point, not the text backstop.
		use std::str::FromStr;
		let values: Vec<BigDecimal> = ["0.1", "0.2", "0.3", "0.4"].iter().map(|s| BigDecimal::from_str(s).unwrap()).collect();
		let est = StorageEstimate::from_values(&values, &BigDecimal::from(0));
		assert_eq!(est.physical_type, "scaled_i64");
		assert!(est.is_exact);
		assert_eq!(est.lossy_count, 0);
		assert_eq!(est.value_count, 4);
		assert_eq!(est.estimated_value_bytes, 4 * 8);
		assert!((est.bytes_per_point - 8.0).abs() < f64::EPSILON);
		assert_eq!(est.max_abs_error, "0");
		assert_eq!(est.tolerance, "0");
		// `from_values` supplies no timestamps, so the total equals the value cost.
		assert_eq!(est.timestamp_bytes, 0);
		assert!((est.timestamp_bytes_per_point - 0.0).abs() < f64::EPSILON);
		assert!((est.total_bytes_per_point - est.bytes_per_point).abs() < f64::EPSILON);
	}

	#[test]
	fn from_columns_picks_bitpack_for_a_regular_timestamp_series() {
		// A regular microsecond series delta-of-deltas to a run of zeros, so the
		// cheapest codec — fixed-width bit-packing at width 0 — wins and is recorded;
		// its cost is the 8-byte anchor + a 1-byte first delta + a 1-byte width
		// header = 10, below the RLE 11 and plain-varint 12, and far below raw 8
		// bytes/point.
		use std::str::FromStr;
		let values: Vec<BigDecimal> = ["0.1", "0.2", "0.3", "0.4", "0.5"].iter().map(|s| BigDecimal::from_str(s).unwrap()).collect();
		let timestamps: Vec<i64> = (0..5).map(|i| 1_000 + i * 10).collect();
		let est = StorageEstimate::from_columns(&values, &timestamps, TimeUnit::Micros, &BigDecimal::from(0));
		assert_eq!(est.timestamp_unit, "micros");
		assert_eq!(est.timestamp_encoding, "delta_of_delta_bitpack");
		assert_eq!(est.timestamp_bytes, 10);
		assert!((est.timestamp_bytes_per_point - 10.0 / 5.0).abs() < f64::EPSILON);
		assert!((est.total_bytes_per_point - (est.bytes_per_point + est.timestamp_bytes_per_point)).abs() < f64::EPSILON);
		// The timestamp column is far cheaper than storing raw 8-byte epochs.
		assert!(est.timestamp_bytes_per_point < 8.0);
	}

	#[test]
	fn realized_value_bytes_agrees_with_the_segment_and_beats_the_naive_estimate() {
		// The realized on-disk value payload the bench now reports must match a sealed
		// Segment under the codec it actually selects, and for a small-mantissa
		// scaled_i64 column it is below the naive estimated_value_bytes — the accurate
		// bytes/point figure. These six small mantissas (1,2,3,5,8,13) bit-pack below
		// the per-value varint, so the selected codec is scaled_bitpack.
		use std::str::FromStr;
		let values: Vec<BigDecimal> = ["0.01", "0.02", "0.03", "0.05", "0.08", "0.13"].iter().map(|s| BigDecimal::from_str(s).unwrap()).collect();
		let timestamps: Vec<i64> = (0..6).map(|i| 1_000 + i * 10).collect();
		let tolerance = BigDecimal::from(0);
		let est = StorageEstimate::from_columns(&values, &timestamps, TimeUnit::Micros, &tolerance);
		assert_eq!(est.physical_type, "scaled_i64", "small tenths pick scaled_i64");
		assert_eq!(est.value_codec, "scaled_bitpack", "small mantissas bit-pack below the varint");
		let seg = dsp_physical_type::Segment::build(&timestamps, &values, TimeUnit::Micros, &tolerance).expect("segment builds");
		assert_eq!(est.realized_value_bytes, seg.serialized_value_bytes(), "bench realized bytes must match the sealed segment");
		assert!(est.realized_value_bytes < est.estimated_value_bytes, "the selected codec realizes below the naive {} estimate", est.estimated_value_bytes);
	}

	#[test]
	fn from_columns_surfaces_gorilla_for_scattered_single_jitter() {
		// The governing rule: a shipped codec advance must show a benchmarked outcome.
		// A regular 1000us base with an isolated jitter every 16th interval (within
		// Gorilla's +/-2048 bucket) is Gorilla's win regime — the bench estimate must
		// name it and cost it below the bit-packed alternative, and must agree with a
		// realized on-disk Segment (proving the win is realized, not just projected).
		use std::str::FromStr;
		let mut timestamps = Vec::with_capacity(1000);
		let mut t = 0_i64;
		for i in 0..1000 {
			t += if i % 16 == 15 { 1_000 + 1_500 } else { 1_000 };
			timestamps.push(t);
		}
		let values: Vec<BigDecimal> = (0..1000).map(|i| BigDecimal::from_str(&format!("{}.5", i % 7)).unwrap()).collect();
		let tolerance = BigDecimal::from(0);
		let est = StorageEstimate::from_columns(&values, &timestamps, TimeUnit::Micros, &tolerance);
		assert_eq!(est.timestamp_encoding, "delta_of_delta_gorilla", "scattered jitter must surface the Gorilla codec in the bench");
		// It agrees with a realized segment written through the .dspseg codec.
		let seg = dsp_physical_type::Segment::build(&timestamps, &values, TimeUnit::Micros, &tolerance).expect("segment builds");
		assert_eq!(est.timestamp_encoding, seg.timestamp_encoding_name());
		assert_eq!(est.timestamp_bytes, seg.timestamp_bytes());
		// Gorilla beats what a fixed-width bit-pack of the same column would cost.
		let dod = encode_delta_of_delta(&timestamps, TimeUnit::Micros);
		assert!(est.timestamp_bytes < dod.bitpack_estimated_bytes(), "gorilla {} must beat bit-pack {}", est.timestamp_bytes, dod.bitpack_estimated_bytes());
	}

	#[test]
	fn storage_estimate_agrees_with_a_real_segment() {
		// The advisory bench estimate and a realized Storage-v2 `Segment` must report
		// the same stored cost for the same data — they share `dsp-physical-type`'s
		// column estimators and codec selector, so this guards against drift.
		use std::str::FromStr;
		let values: Vec<BigDecimal> = ["1.5", "2.25", "3.75", "10.0", "0.5", "7.125"].iter().map(|s| BigDecimal::from_str(s).unwrap()).collect();
		let timestamps: Vec<i64> = (0..6).map(|i| 2_000 + i * 25).collect();
		let tolerance = BigDecimal::from(0);
		let est = StorageEstimate::from_columns(&values, &timestamps, TimeUnit::Micros, &tolerance);
		let seg = dsp_physical_type::Segment::build(&timestamps, &values, TimeUnit::Micros, &tolerance).expect("segment builds");
		assert_eq!(est.physical_type, seg.physical_type().name());
		assert_eq!(est.estimated_value_bytes, seg.value_bytes());
		assert_eq!(est.timestamp_bytes, seg.timestamp_bytes());
		assert_eq!(est.timestamp_encoding, seg.timestamp_encoding_name());
		assert_eq!(est.value_count, seg.row_count());
		assert!((est.total_bytes_per_point - seg.bytes_per_point()).abs() < f64::EPSILON, "bench {} vs segment {}", est.total_bytes_per_point, seg.bytes_per_point());
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
