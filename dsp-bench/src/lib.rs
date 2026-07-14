//! # DSP-Bench
//!
//! A reproducible, correctness-gated benchmark harness for DSP, the first track
//! on the roadmap's benchmark-led commercial thesis. DSP-Bench is both an
//! internal engineering suite and a public/customer-runnable diagnostic: every
//! number it emits ships with the dataset seed, the workload profile, and a
//! correctness verdict, so results are reproducible and trustworthy rather than
//! synthetic wins.
//!
//! ## Shape (this is the scaffold)
//!
//! - [`accuracy`] — reconstruction-quality metrics ([`AccuracyMetrics`]:
//!   RMSE / MAE / max-error / bias) scored against the synthetic profile's known
//!   analytic ground truth, so the reconstruction methods can be compared on
//!   accuracy, not only speed.
//! - [`profile`] — workload profiles + dataset sourcing. The flagship is
//!   [`InterpolationProfile::interpolation_heavy_irregular`] (seeded synthetic);
//!   [`InterpolationProfile::from_line_protocol`] drives the same workload from a
//!   TSBS / `InfluxDB`-Line-Protocol payload.
//! - [`adapter`] — the vendor-neutral [`SystemAdapter`] trait every benchmarked
//!   system is driven through.
//! - [`dsp_adapter`] — the DSP reference adapter ([`DspAdapter`]).
//! - [`baseline_adapter`] — the portable client-side linear baseline
//!   ([`BaselineLinearAdapter`]) DSP is compared against (fair-protocol class C).
//! - [`forward_fill_adapter`] — the portable forward-fill / LOCF baseline
//!   ([`ForwardFillAdapter`]), the in-process mirror of native TSDB
//!   `FILL(previous)` gap-fill (fair-protocol class B reproduced portably).
//! - [`line_protocol`] — `InfluxDB` Line Protocol parsing ([`parse_points`]) for
//!   TSBS-compatible dataset ingest.
//! - [`schema`] — the serializable [`BenchResult`] record (Phase 1.1 latency
//!   distribution + correctness + dataset metadata + end-to-end timing spans).
//! - [`stats`] — p50/p95/p99 latency summarization + seeded bootstrap
//!   confidence intervals ([`LatencyStats::bootstrap_cis`]).
//! - [`report`] — the JSON report runner: a [`BenchReport`] envelope (run
//!   metadata + results) persisted as a durable `reports/json/` artifact.
//!
//! Competitor adapters (`ClickHouse`, `InfluxDB 3`, `QuestDB`, `TimescaleDB`,
//! `DuckDB`), additional workloads, dataset corpora, and report runners land in
//! subsequent increments per the roadmap.

#![warn(clippy::pedantic, clippy::nursery, clippy::all)]
#![allow(clippy::module_name_repetitions)]

pub mod accuracy;
pub mod adapter;
pub mod baseline_adapter;
pub mod dsp_adapter;
pub mod forward_fill_adapter;
pub mod line_protocol;
pub mod point_lookup;
pub mod profile;
pub mod range_fetch;
pub mod report;
pub mod schema;
pub mod stats;

use std::time::Instant;

use bigdecimal::ToPrimitive;
use splimes::generate_target_times;

pub use crate::{
	accuracy::{synthetic_ground_truth, AccuracyError, AccuracyMetrics}, adapter::SystemAdapter, baseline_adapter::BaselineLinearAdapter, dsp_adapter::DspAdapter, forward_fill_adapter::ForwardFillAdapter, line_protocol::{parse, parse_points, FieldValue, LineRecord, ParseError, TimestampPrecision}, point_lookup::{run_point_lookup, LookupMode, PointLookupParams, PointLookupProfile}, profile::{DatasetSource, InterpolationProfile, LineProtocolProfileError, SignalShape, SyntheticParams}, range_fetch::{run_range_fetch, RangeFetchParams, RangeFetchProfile}, report::{BenchReport, RunMetadata}, schema::{BenchResult, CorrectnessReport, DatasetMeta, StorageEstimate, TimingBreakdown, SCHEMA_VERSION}, stats::{BootstrapConfig, ConfidenceInterval, LatencyCis, LatencyStats}
};

/// Workload class label recorded for the interpolation profile.
const WORKLOAD_UPSAMPLE_INTERPOLATE: &str = "upsample_interpolate";

/// Elapsed nanoseconds since `since`, saturated into a `u64` so a pathologically
/// long span can never overflow or wrap the recorded timing.
#[allow(clippy::cast_possible_truncation)]
fn span_ns(since: Instant) -> u64 {
	since.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64
}

/// Run an interpolation profile against an adapter for `reps` timed repetitions
/// and return a fully-populated [`BenchResult`].
///
/// The dataset is generated once (seeded, reproducible); each rep receives a
/// fresh clone so in-place mutation by an adapter never contaminates later reps.
/// Latency is measured around the adapter call only. Correctness is judged on
/// the final rep's output: the produced grid must be the expected size and every
/// value must be finite.
///
/// # Errors
///
/// Returns an error if `reps == 0`, or if any rep's adapter call fails.
pub async fn run_profile<A: SystemAdapter + ?Sized>(adapter: &A, profile: &InterpolationProfile, reps: usize) -> anyhow::Result<BenchResult> {
	anyhow::ensure!(reps > 0, "reps must be > 0");

	// End-to-end span starts at the very top of the timed work (dataset
	// generation included) and is read off again after the last rep, so the
	// reported breakdown accounts for the whole run, not just the adapter calls.
	let run_start = Instant::now();

	let gen_start = Instant::now();
	let dataset = profile.generate();
	let dataset_generation_ns = span_ns(gen_start);
	let input_points = dataset.len();
	let (start, end) = (profile.start(), profile.end());
	let expected_output_points = generate_target_times(start, end, profile.resolution).len();

	let mut samples_ns: Vec<u64> = Vec::with_capacity(reps);
	let mut last_output: Vec<splimes::Point> = Vec::new();

	for _ in 0..reps {
		let mut points = dataset.clone();
		let t0 = Instant::now();
		let output = adapter.interpolate_range(&mut points, start, end, profile.resolution, profile.spline).await?;
		samples_ns.push(span_ns(t0));
		last_output = output;
	}

	// The operation under test is the sum of the timed adapter calls; the
	// end-to-end span is the whole function up to here.
	let measured_ns = samples_ns.iter().copied().fold(0_u64, u64::saturating_add);
	let end_to_end_ns = span_ns(run_start);
	let timing = TimingBreakdown { dataset_generation_ns, measured_ns, end_to_end_ns };

	let actual_output_points = last_output.len();
	let values_finite = last_output.iter().all(|p| p.value.to_f64().is_some_and(f64::is_finite));
	let correctness = CorrectnessReport { output_count_ok: actual_output_points > 0 && actual_output_points == expected_output_points, expected_output_points, actual_output_points, values_finite };

	// Quality alongside speed: when the profile has a known analytic ground truth
	// (synthetic source), score the final rep's reconstruction against it. Reusing
	// `last_output` means no extra adapter run. A line-protocol source has no
	// ground truth (`synthetic_ground_truth` errors) and a grid-size mismatch makes
	// alignment fail — either way accuracy is simply absent, never fatal.
	let accuracy = accuracy::synthetic_ground_truth(profile).ok().and_then(|truth| AccuracyMetrics::from_aligned(&last_output, &truth).ok());

	// North-star storage term: estimate total bytes/point of the *stored* point
	// columns — value under the narrowest lossless physical encoding (tolerance 0,
	// so any loss would be reported, never silent) and timestamp under lossless
	// delta-of-delta + varint coding. The input dataset is the data on disk; the
	// interpolated output is computed on read, not stored. Timestamps are stored
	// as microsecond epochs (the declared unit).
	let stored_values: Vec<bigdecimal::BigDecimal> = dataset.iter().map(|p| p.value.clone()).collect();
	let stored_timestamps: Vec<i64> = dataset.iter().map(|p| p.timestamp.timestamp_micros()).collect();
	let storage = Some(StorageEstimate::from_columns(&stored_values, &stored_timestamps, dsp_physical_type::TimeUnit::Micros, &bigdecimal::BigDecimal::from(0)));

	let latency = LatencyStats::from_samples(&samples_ns);
	// Bootstrap CIs for the latency distribution (fair-protocol Phase 1.1). The
	// resampling seed is derived from the dataset seed so the interval is
	// reproducible yet decoupled from the dataset-generation RNG stream.
	let latency_ci = Some(LatencyStats::bootstrap_cis(&samples_ns, &BootstrapConfig { seed: profile.seed ^ 0x_C0FF_EE15_C0DE_u64, ..BootstrapConfig::default() }));
	let throughput_points_per_sec = if latency.mean_ns > 0 {
		#[allow(clippy::cast_precision_loss)]
		let secs = latency.mean_ns as f64 / 1e9;
		#[allow(clippy::cast_precision_loss)]
		let points = actual_output_points as f64;
		points / secs
	} else {
		0.0
	};

	Ok(BenchResult { schema_version: SCHEMA_VERSION, profile: profile.name.clone(), adapter: adapter.name().to_string(), workload: WORKLOAD_UPSAMPLE_INTERPOLATE.to_string(), reps, dataset: DatasetMeta { input_points, output_points: actual_output_points, irregular: true, missingness_fraction: profile.missingness_fraction, seed: profile.seed, signal_shape: profile.ground_truth_shape() }, latency, latency_ci, throughput_points_per_sec, timing, correctness, accuracy, storage })
}

/// Measure reconstruction *accuracy*: run `adapter` once over `profile` and score
/// its output grid against the profile's known analytic ground truth.
///
/// This is the quality counterpart to [`run_profile`]'s speed measurement. It is
/// only meaningful for a synthetic ([`DatasetSource::Generated`]) profile; a
/// line-protocol profile has no ground truth and yields
/// [`AccuracyError::NoGroundTruth`]. A single run (not `reps`) is enough because
/// accuracy is deterministic for a seed — the reconstruction of a fixed dataset
/// does not vary across repetitions.
///
/// # Errors
///
/// Propagates an adapter failure, or surfaces an [`AccuracyError`] (no ground
/// truth, a length mismatch between the output grid and the truth grid, or a
/// non-finite predicted value) as an `anyhow::Error`.
pub async fn measure_accuracy<A: SystemAdapter + ?Sized>(adapter: &A, profile: &InterpolationProfile) -> anyhow::Result<AccuracyMetrics> {
	let truth = accuracy::synthetic_ground_truth(profile)?;
	let mut points = profile.generate();
	let (start, end) = (profile.start(), profile.end());
	let output = adapter.interpolate_range(&mut points, start, end, profile.resolution, profile.spline).await?;
	Ok(AccuracyMetrics::from_aligned(&output, &truth)?)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[tokio::test]
	async fn dsp_adapter_runs_interpolation_heavy_irregular() {
		let profile = InterpolationProfile::interpolation_heavy_irregular();
		let adapter = DspAdapter::new();
		let result = run_profile(&adapter, &profile, 3).await.expect("benchmark run succeeds");

		assert_eq!(result.adapter, "dsp");
		assert_eq!(result.profile, "interpolation-heavy-irregular");
		assert_eq!(result.workload, "upsample_interpolate");
		assert_eq!(result.reps, 3);
		assert_eq!(result.latency.count, 3);

		// The DSP path must produce a non-empty, finite, correctly-sized grid.
		assert!(result.dataset.output_points > 0, "interpolation produced no points");
		assert!(result.correctness.values_finite, "interpolated values must be finite");
		assert!(result.correctness.passed(), "correctness must pass: {:?}", result.correctness);
		assert!(result.is_publishable(), "result should be publishable");
		assert!(result.throughput_points_per_sec > 0.0, "throughput must be positive");

		// A synthetic profile has a known ground truth, so the run must carry
		// finite accuracy metrics scored over the whole grid.
		let acc = result.accuracy.expect("synthetic run must carry accuracy metrics");
		assert_eq!(acc.count, result.dataset.output_points, "accuracy must score the whole output grid");
		assert!(acc.rmse.is_finite() && acc.mae.is_finite() && acc.max_abs_error.is_finite() && acc.bias.is_finite(), "accuracy metrics must be finite: {acc:?}");
		assert!(acc.max_abs_error >= acc.rmse - 1e-9 && acc.rmse >= acc.mae - 1e-9, "metric invariants must hold: {acc:?}");

		// The run must carry an end-to-end timing breakdown whose spans are
		// internally consistent: the operation under test is non-zero, dataset
		// generation plus the measured calls never exceed the whole-run span, and
		// the measured span equals the sum of the per-rep latencies.
		let timing = result.timing;
		assert!(timing.measured_ns > 0, "measured span must be non-zero");
		assert!(timing.end_to_end_ns >= timing.dataset_generation_ns + timing.measured_ns, "end-to-end span must cover dataset generation + measured work: {timing:?}");
		// `measured_ns` is the raw sum of samples; `mean_ns * count` reconstructs
		// it up to integer-division rounding (the dropped remainder is `< count`),
		// so the two must agree within `count` nanoseconds.
		let reconstructed = result.latency.mean_ns * result.latency.count as u64;
		let drift = timing.measured_ns.abs_diff(reconstructed);
		assert!(drift < result.latency.count as u64 + 1, "measured_ns must match summed per-rep latency within rounding: measured={} reconstructed={} drift={}", timing.measured_ns, reconstructed, drift);

		// The run must carry reproducible bootstrap CIs, each properly ordered.
		let cis = result.latency_ci.as_ref().expect("run must populate latency CIs");
		assert_eq!(cis.resamples, BootstrapConfig::default().resamples);
		for ci in [&cis.mean, &cis.p50, &cis.p95, &cis.p99] {
			assert!(ci.lower_ns <= ci.point_ns && ci.point_ns <= ci.upper_ns, "CI must bracket its point estimate: {ci:?}");
		}

		// Result schema must round-trip through JSON for artifact storage. The
		// exact (integer/string) fields are compared directly; the derived
		// `throughput_points_per_sec` is compared with a tolerance because a
		// real-world `f64` can shift by a ULP across a decimal JSON round-trip.
		let json = serde_json::to_string(&result).expect("serialize result");
		let back: BenchResult = serde_json::from_str(&json).expect("deserialize result");
		assert_eq!(result.schema_version, back.schema_version);
		assert_eq!(result.profile, back.profile);
		assert_eq!(result.adapter, back.adapter);
		assert_eq!(result.reps, back.reps);
		assert_eq!(result.dataset, back.dataset);
		assert_eq!(result.latency, back.latency);
		assert_eq!(result.latency_ci, back.latency_ci);
		assert_eq!(result.timing, back.timing);
		assert_eq!(result.correctness, back.correctness);
		// The integer `count` must match exactly; the `f64` metrics are compared with
		// a tolerance because a real-world `f64` can shift by a ULP across a decimal
		// JSON round-trip (same reason `throughput` is compared loosely below).
		let (ra, ba) = (result.accuracy.expect("synthetic run has accuracy"), back.accuracy.expect("round-trip keeps accuracy"));
		assert_eq!(ra.count, ba.count, "accuracy point count must survive the round-trip");
		for (a, b, name) in [(ra.rmse, ba.rmse, "rmse"), (ra.mae, ba.mae, "mae"), (ra.max_abs_error, ba.max_abs_error, "max"), (ra.bias, ba.bias, "bias")] {
			assert!((a - b).abs() < 1e-9, "accuracy.{name} must survive the round-trip within tolerance: {a} vs {b}");
		}
		let throughput_drift = (result.throughput_points_per_sec - back.throughput_points_per_sec).abs();
		assert!(throughput_drift < 1e-6, "throughput must survive round-trip within tolerance, drifted {throughput_drift}");
	}

	#[tokio::test]
	async fn run_result_persists_as_a_json_report_artifact() {
		use crate::report::{default_filename, BenchReport, RunMetadata};

		let profile = InterpolationProfile::interpolation_heavy_irregular();
		let adapter = DspAdapter::new();
		let result = run_profile(&adapter, &profile, 3).await.expect("benchmark run succeeds");

		let metadata = RunMetadata::capture("2026-06-06T00:00:00+00:00".to_string());
		let report = BenchReport::with_results(metadata, vec![result]);
		assert!(report.is_publishable(), "a passing DSP run yields a publishable report");

		// Persist the artifact to a unique temp path and read it straight back, so
		// the whole run -> report -> disk -> parse path is exercised end to end.
		let mut path = std::env::temp_dir();
		path.push(format!("dsp-bench-e2e-{}", std::process::id()));
		path.push(default_filename(&report.results[0].profile, &report.results[0].adapter));

		report.write_json(&path).expect("write report artifact");
		let raw = std::fs::read_to_string(&path).expect("read report artifact");
		let back: BenchReport = serde_json::from_str(&raw).expect("parse report artifact");
		assert_eq!(back.results.len(), 1);
		assert_eq!(back.results[0].adapter, "dsp");
		assert!(back.is_publishable());

		let _ = std::fs::remove_dir_all(path.parent().unwrap_or(&path));
	}

	#[tokio::test]
	async fn dsp_adapter_runs_a_line_protocol_sourced_profile() {
		// A TSBS-style ILP payload: irregular, out-of-order, second-precision stamps
		// spanning ten minutes. Driving it through `from_line_protocol` must produce
		// a result indistinguishable in shape from the synthetic flagship run — same
		// harness, same correctness gate, same reporting — just a different source.
		let payload = "\
cpu,host=h0 usage=10.0 0\n\
cpu,host=h0 usage=14.5 180\n\
cpu,host=h0 usage=12.0 60\n\
cpu,host=h0 usage=11.5 240\n\
cpu,host=h0 usage=13.0 120\n\
cpu,host=h0 usage=15.5 360\n\
cpu,host=h0 usage=16.0 480\n\
cpu,host=h0 usage=14.0 600\n";
		let profile = InterpolationProfile::from_line_protocol("tsbs-cpu-usage", payload, "usage", TimestampPrecision::Seconds, splimes::Spline::Cubic, splimes::Resolution::Minutes).expect("payload builds a profile");

		let adapter = DspAdapter::new();
		let result = run_profile(&adapter, &profile, 3).await.expect("ILP-sourced benchmark run succeeds");

		assert_eq!(result.adapter, "dsp");
		assert_eq!(result.profile, "tsbs-cpu-usage");
		assert_eq!(result.workload, "upsample_interpolate");
		assert_eq!(result.dataset.input_points, 8, "all eight ILP records reach the harness");
		assert_eq!(result.dataset.seed, 0, "a line-protocol source carries no synthetic seed");

		// The full correctness gate must pass on real-sourced data exactly as it does
		// on synthetic data: a non-empty, correctly-sized, finite interpolated grid.
		assert!(result.dataset.output_points > 0, "interpolation produced no points");
		assert!(result.correctness.passed(), "correctness must pass: {:?}", result.correctness);
		assert!(result.is_publishable(), "an ILP-sourced run should be publishable");
		assert!(result.throughput_points_per_sec > 0.0, "throughput must be positive");
		assert!(result.timing.measured_ns > 0, "measured span must be non-zero");
		assert!(result.accuracy.is_none(), "a line-protocol source has no ground truth, so no accuracy");
	}

	#[tokio::test]
	async fn measure_accuracy_scores_every_reconstruction_against_ground_truth() {
		let profile = InterpolationProfile::interpolation_heavy_irregular();
		let expected = generate_target_times(profile.start(), profile.end(), profile.resolution).len();

		// Every in-process reconstruction method must yield finite, grid-aligned
		// accuracy metrics obeying the universal error-statistic invariants
		// (max >= rmse >= mae >= 0, |bias| <= mae). We do not assert one method
		// beats another: with seeded noise that ordering is a real result to
		// report, not a property to bake into a test.
		let dsp = measure_accuracy(&DspAdapter::new(), &profile).await.expect("dsp accuracy");
		let linear = measure_accuracy(&BaselineLinearAdapter::new(), &profile).await.expect("linear accuracy");
		let locf = measure_accuracy(&ForwardFillAdapter::new(), &profile).await.expect("forward-fill accuracy");

		for (label, m) in [("dsp", dsp), ("linear", linear), ("forward-fill", locf)] {
			assert_eq!(m.count, expected, "{label}: accuracy must score the whole grid");
			assert!(m.rmse.is_finite() && m.mae.is_finite() && m.max_abs_error.is_finite() && m.bias.is_finite(), "{label}: metrics must be finite: {m:?}");
			assert!(m.max_abs_error >= m.rmse - 1e-9, "{label}: max {} >= rmse {}", m.max_abs_error, m.rmse);
			assert!(m.rmse >= m.mae - 1e-9, "{label}: rmse {} >= mae {}", m.rmse, m.mae);
			assert!(m.mae >= 0.0, "{label}: mae must be non-negative");
			assert!(m.bias.abs() <= m.mae + 1e-9, "{label}: |bias| {} <= mae {}", m.bias.abs(), m.mae);
		}
	}

	#[tokio::test]
	async fn noise_free_data_reconstructs_far_more_accurately_than_noisy() {
		// Same seed and geometry, only the noise amplitude differs. Measurement noise
		// is error no reconstruction can remove, so the clean run must score strictly
		// better — and a well-sampled smooth signal should reconstruct with low RMSE.
		// This is the regime where DSP's spline path is meant to shine.
		let clean = InterpolationProfile::synthetic("clean", SyntheticParams { seed: 7, noise_amplitude: 0.0, ..SyntheticParams::default() });
		let noisy = InterpolationProfile::synthetic("noisy", SyntheticParams { seed: 7, noise_amplitude: 2.0, ..SyntheticParams::default() });

		let clean_acc = measure_accuracy(&DspAdapter::new(), &clean).await.expect("clean accuracy");
		let noisy_acc = measure_accuracy(&DspAdapter::new(), &noisy).await.expect("noisy accuracy");

		assert!(clean_acc.rmse < noisy_acc.rmse, "clean data must reconstruct more accurately than noisy: clean {} vs noisy {}", clean_acc.rmse, noisy_acc.rmse);
		assert!(clean_acc.rmse < 1.5, "cubic should recover a noise-free smooth signal with low error, got rmse {}", clean_acc.rmse);
	}

	#[tokio::test]
	async fn measure_accuracy_rejects_a_line_protocol_profile() {
		// A real-world ILP source has no analytic ground truth to score against.
		let payload = "cpu,host=h0 usage=10.0 0\ncpu,host=h0 usage=12.0 120\n";
		let profile = InterpolationProfile::from_line_protocol("tsbs-cpu", payload, "usage", TimestampPrecision::Seconds, splimes::Spline::Cubic, splimes::Resolution::Minutes).expect("valid payload");
		let err = measure_accuracy(&DspAdapter::new(), &profile).await.expect_err("no ground truth must error");
		assert!(err.downcast_ref::<AccuracyError>().is_some_and(|e| *e == AccuracyError::NoGroundTruth), "expected NoGroundTruth, got {err:?}");
	}

	#[tokio::test]
	async fn run_profile_records_the_signal_shape_and_accuracy_for_every_shape() {
		// The full runner must thread each selectable ground-truth shape through to
		// the artifact (`dataset.signal_shape`) and still score accuracy against it.
		// This locks the end-to-end path the unit tests only cover piecewise.
		for shape in [SignalShape::MultiSine, SignalShape::Sawtooth, SignalShape::Step, SignalShape::DampedSine] {
			let profile = InterpolationProfile::synthetic("shape-sweep", SyntheticParams { signal_shape: shape, noise_amplitude: 0.0, ..SyntheticParams::default() });
			let result = run_profile(&DspAdapter::new(), &profile, 3).await.expect("run completes");

			assert_eq!(result.dataset.signal_shape, Some(shape), "the artifact must record the generated shape");
			assert!(result.correctness.passed(), "{shape:?}: correctness must pass: {:?}", result.correctness);
			let accuracy = result.accuracy.expect("a synthetic run carries accuracy");
			assert!(accuracy.rmse.is_finite() && accuracy.mae.is_finite() && accuracy.max_abs_error.is_finite() && accuracy.bias.is_finite(), "{shape:?}: accuracy metrics must be finite: {accuracy:?}");
			assert!(accuracy.count > 0, "{shape:?}: accuracy must score the grid");
		}
	}

	#[tokio::test]
	async fn zero_reps_is_rejected() {
		let profile = InterpolationProfile::interpolation_heavy_irregular();
		let adapter = DspAdapter::new();
		let err = run_profile(&adapter, &profile, 0).await;
		assert!(err.is_err(), "zero reps must be rejected");
	}
}
