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

	/// The most accurate *publishable* result in the report: the one with the
	/// smallest RMSE against ground truth, among results that carry accuracy
	/// metrics (synthetic profiles) and passed correctness.
	///
	/// Returns `None` when no publishable result has accuracy — e.g. a
	/// line-protocol report (no ground truth) or an all-failing one. A result
	/// whose RMSE is `NaN` is skipped rather than allowed to win a comparison it
	/// cannot meaningfully participate in.
	#[must_use]
	pub fn most_accurate(&self) -> Option<&BenchResult> {
		self.results.iter().filter(|r| r.is_publishable()).filter_map(|r| r.accuracy.map(|a| (r, a.rmse))).filter(|(_, rmse)| rmse.is_finite()).min_by(|(_, a), (_, b)| a.total_cmp(b)).map(|(r, _)| r)
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

	/// Render the report as a single self-contained HTML document (no external
	/// assets or scripts) — the roadmap's `reports/html/` slice, a human-readable
	/// view of the same results the JSON artifact carries. Every system appears in
	/// one table: dataset shape, latency percentiles, throughput, the correctness
	/// verdict, and (for synthetic runs) reconstruction accuracy, with the
	/// most-accurate row highlighted. All caller-supplied strings are HTML-escaped.
	#[must_use]
	pub fn to_html(&self) -> String {
		let m = &self.metadata;
		let best = self.most_accurate();
		let rows: String = self.results.iter().map(|r| result_row_html(r, best.is_some_and(|b| std::ptr::eq(b, r)))).collect();
		let version = escape_html(&m.dsp_bench_version);
		let os = escape_html(&m.os);
		let arch = escape_html(&m.arch);
		let generated = escape_html(if m.generated_at.is_empty() { "(unstamped)" } else { m.generated_at.as_str() });
		let publishable = if self.is_publishable() { "yes" } else { "no" };
		format!("<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<title>DSP-Bench report</title>\n<style>{style}</style>\n</head>\n<body>\n<h1>DSP-Bench report</h1>\n<p class=\"meta\">dsp-bench {version} \u{b7} {os}/{arch} \u{b7} schema v{schema} \u{b7} generated {generated} \u{b7} publishable: {publishable}</p>\n<table>\n<thead><tr>{head}</tr></thead>\n<tbody>\n{rows}</tbody>\n</table>\n</body>\n</html>\n", style = HTML_STYLE, schema = self.schema_version, head = HTML_HEAD_CELLS)
	}

	/// Write the report as a self-contained HTML document to `path`, creating any
	/// missing parent directories first.
	///
	/// # Errors
	///
	/// Returns an error if a parent directory cannot be created or the file cannot
	/// be written.
	pub fn write_html(&self, path: impl AsRef<Path>) -> io::Result<()> {
		let path = path.as_ref();
		if let Some(parent) = path.parent() {
			if !parent.as_os_str().is_empty() {
				fs::create_dir_all(parent)?;
			}
		}
		fs::write(path, self.to_html())
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

/// Build a filesystem-safe HTML artifact filename for a `(profile, adapter)`
/// pair, e.g. `interpolation-heavy-irregular__dsp.html` — the HTML sibling of
/// [`default_filename`].
#[must_use]
pub fn default_html_filename(profile: &str, adapter: &str) -> String {
	format!("{}__{}.html", sanitize_component(profile), sanitize_component(adapter))
}

/// Inline stylesheet for [`BenchReport::to_html`]. Kept tiny and self-contained so
/// the artifact needs no external assets.
const HTML_STYLE: &str = "body{font-family:system-ui,sans-serif;margin:2rem;color:#1a1a1a}h1{font-size:1.4rem}.meta{color:#555;font-size:.9rem}table{border-collapse:collapse;margin-top:1rem;font-size:.9rem}th,td{border:1px solid #ccc;padding:.3rem .6rem;text-align:right}th:first-child,td:first-child,th:nth-child(2),td:nth-child(2){text-align:left}thead{background:#f0f0f0}tr.best{background:#e7f7e7;font-weight:600}";

/// Table header cells for [`BenchReport::to_html`], matching [`result_row_html`].
const HTML_HEAD_CELLS: &str = "<th>adapter</th><th>shape</th><th>in</th><th>out</th><th>p50 ms</th><th>p95 ms</th><th>p99 ms</th><th>mean ms</th><th>pts/s</th><th>correct</th><th>rmse</th><th>mae</th><th>max</th><th>bias</th>";

/// Nanoseconds rendered as fractional milliseconds for display.
#[allow(clippy::cast_precision_loss)]
fn ms(ns: u64) -> f64 {
	ns as f64 / 1e6
}

/// Render one result as an HTML table row. `is_best` tags the most-accurate row.
fn result_row_html(r: &BenchResult, is_best: bool) -> String {
	let l = &r.latency;
	let adapter = escape_html(&r.adapter);
	let shape = r.dataset.signal_shape.map_or_else(|| "&mdash;".to_string(), |s| format!("{s:?}"));
	let correctness = if r.correctness.passed() { "PASS" } else { "FAIL" };
	// Accuracy is present only for a synthetic profile; otherwise the four cells
	// are em-dashes so the column stays aligned.
	let accuracy = r.accuracy.map_or_else(|| "<td>&mdash;</td><td>&mdash;</td><td>&mdash;</td><td>&mdash;</td>".to_string(), |a| format!("<td>{:.4}</td><td>{:.4}</td><td>{:.4}</td><td>{:+.4}</td>", a.rmse, a.mae, a.max_abs_error, a.bias));
	let cls = if is_best { " class=\"best\"" } else { "" };
	format!("<tr{cls}><td>{adapter}</td><td>{shape}</td><td>{in_pts}</td><td>{out_pts}</td><td>{p50:.3}</td><td>{p95:.3}</td><td>{p99:.3}</td><td>{mean:.3}</td><td>{tput:.0}</td><td>{correctness}</td>{accuracy}</tr>\n", in_pts = r.dataset.input_points, out_pts = r.dataset.output_points, p50 = ms(l.p50_ns), p95 = ms(l.p95_ns), p99 = ms(l.p99_ns), mean = ms(l.mean_ns), tput = r.throughput_points_per_sec)
}

/// Escape the five HTML-significant characters so caller-supplied strings (adapter
/// and profile names, captured metadata) cannot break or inject into the document.
fn escape_html(s: &str) -> String {
	let mut out = String::with_capacity(s.len());
	for c in s.chars() {
		match c {
			'&' => out.push_str("&amp;"),
			'<' => out.push_str("&lt;"),
			'>' => out.push_str("&gt;"),
			'"' => out.push_str("&quot;"),
			'\'' => out.push_str("&#39;"),
			other => out.push(other),
		}
	}
	out
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
		BenchResult { schema_version: SCHEMA_VERSION, profile: "interpolation-heavy-irregular".to_string(), adapter: adapter.to_string(), workload: "upsample_interpolate".to_string(), reps: 3, dataset: DatasetMeta { input_points: 200, output_points: 1000, irregular: true, missingness_fraction: 0.2, seed: 7, signal_shape: None }, latency: LatencyStats::from_samples(&[100, 200, 300]), latency_ci: None, throughput_points_per_sec: 5_000_000.0, timing: crate::schema::TimingBreakdown { dataset_generation_ns: 5_000, measured_ns: 600, end_to_end_ns: 6_200 }, correctness: CorrectnessReport { output_count_ok: publishable, expected_output_points: 1000, actual_output_points: if publishable { 1000 } else { 0 }, values_finite: publishable }, accuracy: None }
	}

	/// A publishable result for `adapter` carrying an accuracy report with the given
	/// RMSE (the other accuracy fields are irrelevant to `most_accurate`).
	fn result_with_rmse(adapter: &str, rmse: f64) -> BenchResult {
		let mut r = sample_result(adapter, true);
		r.accuracy = Some(crate::accuracy::AccuracyMetrics { count: 1000, rmse, mae: rmse, max_abs_error: rmse, bias: 0.0 });
		r
	}

	fn metadata() -> RunMetadata {
		RunMetadata { dsp_bench_version: "0.1.0".to_string(), os: "testos".to_string(), arch: "testarch".to_string(), generated_at: "2026-06-06T00:00:00+00:00".to_string() }
	}

	#[test]
	fn most_accurate_picks_the_lowest_finite_rmse_publishable_result() {
		let report = BenchReport::with_results(metadata(), vec![result_with_rmse("dsp", 1.9), result_with_rmse("baseline-linear", 1.2), result_with_rmse("baseline-forward-fill", 2.9)]);
		assert_eq!(report.most_accurate().map(|r| r.adapter.as_str()), Some("baseline-linear"), "the smallest RMSE must win");
	}

	#[test]
	fn most_accurate_ignores_results_without_accuracy_or_failing_correctness() {
		// A line-protocol-style result (no accuracy) and a failing result must be
		// skipped; only the one publishable result that carries accuracy can win.
		let mut failing = result_with_rmse("dsp-broken", 0.1);
		failing.correctness.values_finite = false; // makes it non-publishable
		let report = BenchReport::with_results(metadata(), vec![sample_result("ilp-dsp", true), failing, result_with_rmse("baseline-linear", 1.2)]);
		assert_eq!(report.most_accurate().map(|r| r.adapter.as_str()), Some("baseline-linear"));
	}

	#[test]
	fn most_accurate_is_none_when_no_publishable_result_has_accuracy() {
		// All results are line-protocol-style (no accuracy) -> no winner.
		let report = BenchReport::with_results(metadata(), vec![sample_result("dsp", true), sample_result("baseline-linear", true)]);
		assert!(report.most_accurate().is_none());
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

	#[test]
	fn default_html_filename_mirrors_json_with_an_html_extension() {
		assert_eq!(default_html_filename("interpolation-heavy-irregular", "dsp"), "interpolation-heavy-irregular__dsp.html");
		assert_eq!(default_html_filename("range fetch/v2", "Influx DB 3"), "range-fetch-v2__Influx-DB-3.html");
	}

	#[test]
	fn to_html_renders_a_document_with_one_row_per_result() {
		let report = BenchReport::with_results(metadata(), vec![result_with_rmse("dsp", 1.9), result_with_rmse("baseline-linear", 1.2)]);
		let html = report.to_html();
		assert!(html.starts_with("<!doctype html>"), "must be a full HTML document");
		assert!(html.contains("DSP-Bench report"));
		// Metadata is surfaced in the header.
		assert!(html.contains("testos/testarch"), "metadata must appear: {html}");
		// Every adapter gets a row.
		assert!(html.contains("<td>dsp</td>"), "dsp row missing");
		assert!(html.contains("<td>baseline-linear</td>"), "baseline-linear row missing");
		// Two data rows.
		assert_eq!(html.matches("<tr").count(), 3, "one header row + two data rows expected");
	}

	#[test]
	fn to_html_marks_exactly_the_most_accurate_row() {
		let report = BenchReport::with_results(metadata(), vec![result_with_rmse("dsp", 1.9), result_with_rmse("baseline-linear", 1.2)]);
		let html = report.to_html();
		// Exactly one row is the best, and it is the lowest-RMSE adapter.
		assert_eq!(html.matches("class=\"best\"").count(), 1, "exactly one best row");
		assert!(html.contains("<tr class=\"best\"><td>baseline-linear</td>"), "the lowest-RMSE adapter must be marked best: {html}");

		// With no accuracy anywhere, no row is marked.
		let plain = BenchReport::with_results(metadata(), vec![sample_result("dsp", true)]);
		assert_eq!(plain.to_html().matches("class=\"best\"").count(), 0, "no accuracy -> no best row");
	}

	#[test]
	fn to_html_escapes_caller_supplied_strings() {
		// An adapter name carrying HTML-significant characters must be escaped, never
		// emitted raw — the document is self-contained and must not be injectable.
		let report = BenchReport::with_results(metadata(), vec![sample_result("<script>x</script>", true)]);
		let html = report.to_html();
		assert!(html.contains("&lt;script&gt;x&lt;/script&gt;"), "adapter name must be escaped: {html}");
		assert!(!html.contains("<script>x</script>"), "raw markup must not survive: {html}");
	}

	#[test]
	fn write_html_creates_parents_and_writes_a_document() {
		let report = BenchReport::with_results(metadata(), vec![result_with_rmse("dsp", 1.2)]);
		let mut dir = std::env::temp_dir();
		dir.push(format!("dsp-bench-html-test-{}", std::process::id()));
		dir.push("html");
		let path = dir.join(default_html_filename("interpolation-heavy-irregular", "dsp"));

		report.write_html(&path).expect("write html report");
		let raw = fs::read_to_string(&path).expect("read back html report");
		assert!(raw.starts_with("<!doctype html>"));
		assert!(raw.contains("<td>dsp</td>"));

		let _ = fs::remove_dir_all(dir.parent().unwrap_or(&dir));
	}
}
