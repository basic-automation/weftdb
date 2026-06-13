//! JSON report artifacts for DSP-Bench runs.
//!
//! The roadmap's fair-protocol and anti-Goodhart rules require that every number
//! ship as a durable, inspectable artifact — "keep raw results" — so a run can be
//! reproduced and audited rather than trusted on faith. This module is the first
//! report runner (the `reports/json/` slice): it wraps one or more
//! [`BenchResult`]s in a [`BenchReport`] envelope carrying lightweight run
//! metadata and writes it as pretty-printed JSON.
//!
//! This is intentionally a *minimal* environment capture — `dsp-bench` version,
//! OS, and CPU architecture. The full hardware/system block required by the
//! benchmark-report template (CPU model, RAM, disk, GPU, driver versions, cloud
//! instance + cost) is a later increment; what is captured here is honest about
//! its own scope and never claims more than it measures.

use std::{fs, io, path::Path};

use serde::{Deserialize, Serialize};

use crate::schema::{BenchResult, SCHEMA_VERSION};

/// Lightweight description of the environment a report was produced in.
///
/// Deliberately small and dependency-free: it records only what can be observed
/// without probing hardware. Richer hardware fields are added alongside the
/// instrumentation work that can populate them honestly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunMetadata {
	/// Version of the `dsp-bench` crate that produced the report.
	pub dsp_bench_version: String,
	/// Target operating system (`std::env::consts::OS`, e.g. `windows`, `linux`).
	pub os: String,
	/// Target CPU architecture (`std::env::consts::ARCH`, e.g. `x86_64`).
	pub arch: String,
	/// When the report was generated, RFC 3339 / ISO 8601. Empty when the caller
	/// constructs metadata without a timestamp (e.g. for reproducible tests).
	pub generated_at: String,
}

impl RunMetadata {
	/// Capture the current environment, stamping `generated_at` with `now`.
	///
	/// The timestamp is passed in rather than read from the clock so report
	/// construction stays deterministic and testable; callers that want wall
	/// time pass `chrono::Utc::now().to_rfc3339()`.
	#[must_use]
	pub fn capture(generated_at: String) -> Self {
		Self { dsp_bench_version: env!("CARGO_PKG_VERSION").to_string(), os: std::env::consts::OS.to_string(), arch: std::env::consts::ARCH.to_string(), generated_at }
	}
}

/// A DSP-Bench report: run metadata plus the results gathered in one session.
///
/// One report may hold several [`BenchResult`]s — e.g. the same profile run
/// against multiple adapters, or several profiles in one sweep — so a whole
/// comparison persists as a single self-describing artifact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchReport {
	/// Schema version this report was written with (mirrors [`SCHEMA_VERSION`]).
	pub schema_version: u32,
	/// Environment the results were measured in.
	pub metadata: RunMetadata,
	/// The benchmark results in this report.
	pub results: Vec<BenchResult>,
}

impl BenchReport {
	/// Create an empty report for the given environment.
	#[must_use]
	pub const fn new(metadata: RunMetadata) -> Self {
		Self { schema_version: SCHEMA_VERSION, metadata, results: Vec::new() }
	}

	/// Create a report already populated with `results`.
	#[must_use]
	pub const fn with_results(metadata: RunMetadata, results: Vec<BenchResult>) -> Self {
		Self { schema_version: SCHEMA_VERSION, metadata, results }
	}

	/// Append a result to the report.
	pub fn push(&mut self, result: BenchResult) {
		self.results.push(result);
	}

	/// Number of results that are individually fit to publish.
	#[must_use]
	pub fn publishable_count(&self) -> usize {
		self.results.iter().filter(|r| r.is_publishable()).count()
	}

	/// True when the report holds at least one result and every result is
	/// publishable (correctness passed). A report with a single failing result
	/// is not publishable — honesty gates the whole artifact.
	#[must_use]
	pub fn is_publishable(&self) -> bool {
		!self.results.is_empty() && self.results.iter().all(BenchResult::is_publishable)
	}

	/// Serialize the report to pretty-printed JSON.
	///
	/// # Errors
	///
	/// Returns an error if serialization fails (which, for these plain data
	/// types, indicates a serde configuration bug rather than bad input).
	pub fn to_json_pretty(&self) -> serde_json::Result<String> {
		serde_json::to_string_pretty(self)
	}

	/// Write the report as pretty-printed JSON to `path`, creating any missing
	/// parent directories first.
	///
	/// # Errors
	///
	/// Returns an error if serialization fails, if a parent directory cannot be
	/// created, or if the file cannot be written.
	pub fn write_json(&self, path: impl AsRef<Path>) -> io::Result<()> {
		let path = path.as_ref();
		if let Some(parent) = path.parent() {
			if !parent.as_os_str().is_empty() {
				fs::create_dir_all(parent)?;
			}
		}
		let json = self.to_json_pretty().map_err(io::Error::other)?;
		fs::write(path, json)
	}
}

/// Build a filesystem-safe artifact filename for a `(profile, adapter)` pair,
/// e.g. `interpolation-heavy-irregular__dsp.json`.
///
/// Any character outside `[A-Za-z0-9._-]` is replaced with `-` so the name is
/// portable across filesystems.
#[must_use]
pub fn default_filename(profile: &str, adapter: &str) -> String {
	format!("{}__{}.json", sanitize_component(profile), sanitize_component(adapter))
}

/// Replace any character that is not alphanumeric, `.`, `_`, or `-` with `-`.
fn sanitize_component(s: &str) -> String {
	let cleaned: String = s.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' }).collect();
	if cleaned.is_empty() {
		"unnamed".to_string()
	} else {
		cleaned
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		schema::{CorrectnessReport, DatasetMeta}, stats::LatencyStats
	};

	fn sample_result(adapter: &str, publishable: bool) -> BenchResult {
		BenchResult { schema_version: SCHEMA_VERSION, profile: "interpolation-heavy-irregular".to_string(), adapter: adapter.to_string(), workload: "upsample_interpolate".to_string(), reps: 3, dataset: DatasetMeta { input_points: 200, output_points: 1000, irregular: true, missingness_fraction: 0.2, seed: 7 }, latency: LatencyStats::from_samples(&[100, 200, 300]), latency_ci: None, throughput_points_per_sec: 5_000_000.0, timing: crate::schema::TimingBreakdown { dataset_generation_ns: 5_000, measured_ns: 600, end_to_end_ns: 6_200 }, correctness: CorrectnessReport { output_count_ok: publishable, expected_output_points: 1000, actual_output_points: if publishable { 1000 } else { 0 }, values_finite: publishable }, accuracy: None }
	}

	fn metadata() -> RunMetadata {
		RunMetadata { dsp_bench_version: "0.1.0".to_string(), os: "testos".to_string(), arch: "testarch".to_string(), generated_at: "2026-06-06T00:00:00+00:00".to_string() }
	}

	#[test]
	fn capture_fills_version_and_target() {
		let meta = RunMetadata::capture("2026-06-06T00:00:00+00:00".to_string());
		assert_eq!(meta.dsp_bench_version, env!("CARGO_PKG_VERSION"));
		assert_eq!(meta.os, std::env::consts::OS);
		assert_eq!(meta.arch, std::env::consts::ARCH);
		assert!(!meta.dsp_bench_version.is_empty());
	}

	#[test]
	fn report_round_trips_through_json() {
		let report = BenchReport::with_results(metadata(), vec![sample_result("dsp", true), sample_result("duckdb", true)]);
		let json = report.to_json_pretty().expect("serialize");
		let back: BenchReport = serde_json::from_str(&json).expect("deserialize");
		assert_eq!(report, back);
		assert_eq!(back.schema_version, SCHEMA_VERSION);
		assert_eq!(back.results.len(), 2);
	}

	#[test]
	fn publishability_requires_all_results_to_pass() {
		let mut report = BenchReport::new(metadata());
		assert!(!report.is_publishable(), "empty report is not publishable");

		report.push(sample_result("dsp", true));
		assert!(report.is_publishable());
		assert_eq!(report.publishable_count(), 1);

		report.push(sample_result("broken", false));
		assert!(!report.is_publishable(), "one failing result must fail the report");
		assert_eq!(report.publishable_count(), 1, "the passing result is still counted");
	}

	#[test]
	fn write_json_creates_parents_and_round_trips_from_disk() {
		let report = BenchReport::with_results(metadata(), vec![sample_result("dsp", true)]);
		let mut dir = std::env::temp_dir();
		dir.push(format!("dsp-bench-report-test-{}", std::process::id()));
		dir.push("json");
		let path = dir.join(default_filename("interpolation-heavy-irregular", "dsp"));

		report.write_json(&path).expect("write report");
		let raw = fs::read_to_string(&path).expect("read back report");
		let back: BenchReport = serde_json::from_str(&raw).expect("parse report");
		assert_eq!(report, back);

		// Clean up the temp tree; ignore errors so a leaked temp file never fails
		// the test.
		let _ = fs::remove_dir_all(dir.parent().unwrap_or(&dir));
	}

	#[test]
	fn default_filename_is_sanitized() {
		assert_eq!(default_filename("interpolation-heavy-irregular", "dsp"), "interpolation-heavy-irregular__dsp.json");
		assert_eq!(default_filename("range fetch/v2", "Influx DB 3"), "range-fetch-v2__Influx-DB-3.json");
		assert_eq!(default_filename("", ""), "unnamed__unnamed.json");
	}
}
