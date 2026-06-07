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
//! - [`profile`] — workload profiles + seeded dataset generation. The flagship
//!   is [`InterpolationProfile::interpolation_heavy_irregular`].
//! - [`adapter`] — the vendor-neutral [`SystemAdapter`] trait every benchmarked
//!   system is driven through.
//! - [`dsp_adapter`] — the DSP reference adapter ([`DspAdapter`]).
//! - [`schema`] — the serializable [`BenchResult`] record (Phase 1.1 latency
//!   distribution + correctness + dataset metadata).
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

pub mod adapter;
pub mod dsp_adapter;
pub mod profile;
pub mod report;
pub mod schema;
pub mod stats;

use std::time::Instant;

use bigdecimal::ToPrimitive;
use splimes::generate_target_times;

pub use crate::{
	adapter::SystemAdapter, dsp_adapter::DspAdapter, profile::InterpolationProfile, report::{BenchReport, RunMetadata}, schema::{BenchResult, CorrectnessReport, DatasetMeta, SCHEMA_VERSION}, stats::{BootstrapConfig, ConfidenceInterval, LatencyCis, LatencyStats}
};

/// Workload class label recorded for the interpolation profile.
const WORKLOAD_UPSAMPLE_INTERPOLATE: &str = "upsample_interpolate";

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

	let dataset = profile.generate();
	let input_points = dataset.len();
	let (start, end) = (profile.start(), profile.end());
	let expected_output_points = generate_target_times(start, end, profile.resolution).len();

	let mut samples_ns: Vec<u64> = Vec::with_capacity(reps);
	let mut last_output: Vec<splimes::Point> = Vec::new();

	for _ in 0..reps {
		let mut points = dataset.clone();
		let t0 = Instant::now();
		let output = adapter.interpolate_range(&mut points, start, end, profile.resolution, profile.spline).await?;
		let elapsed = t0.elapsed();
		#[allow(clippy::cast_possible_truncation)]
		samples_ns.push(elapsed.as_nanos().min(u128::from(u64::MAX)) as u64);
		last_output = output;
	}

	let actual_output_points = last_output.len();
	let values_finite = last_output.iter().all(|p| p.value.to_f64().is_some_and(f64::is_finite));
	let correctness = CorrectnessReport { output_count_ok: actual_output_points > 0 && actual_output_points == expected_output_points, expected_output_points, actual_output_points, values_finite };

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

	Ok(BenchResult { schema_version: SCHEMA_VERSION, profile: profile.name.clone(), adapter: adapter.name().to_string(), workload: WORKLOAD_UPSAMPLE_INTERPOLATE.to_string(), reps, dataset: DatasetMeta { input_points, output_points: actual_output_points, irregular: true, missingness_fraction: profile.missingness_fraction, seed: profile.seed }, latency, latency_ci, throughput_points_per_sec, correctness })
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
		assert_eq!(result.correctness, back.correctness);
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
	async fn zero_reps_is_rejected() {
		let profile = InterpolationProfile::interpolation_heavy_irregular();
		let adapter = DspAdapter::new();
		let err = run_profile(&adapter, &profile, 0).await;
		assert!(err.is_err(), "zero reps must be rejected");
	}
}
