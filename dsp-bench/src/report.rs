//! JSON report artifacts for DSP-Bench runs.
//!
//! The roadmap's fair-protocol and anti-Goodhart rules require that every number
//! ship as a durable, inspectable artifact — "keep raw results" — so a run can be
//! reproduced and audited rather than trusted on faith. This module is the first
//! report runner (the `reports/json/` slice): it wraps one or more
//! [`BenchResult`]s in a [`BenchReport`] envelope carrying lightweight run
//! metadata and writes it as pretty-printed JSON.
//!
//! The environment capture records `dsp-bench` version, OS, and CPU architecture,
//! plus a best-effort hardware probe (CPU model, physical/logical core counts,
//! total RAM) toward the benchmark-report template's hardware block. The remaining
//! template fields (disk, GPU, driver versions, cloud instance + cost) are a later
//! increment; every field is honest about its scope — the hardware facts are
//! `Option` and omitted when unread, never claiming more than was measured.
//!
//! Reports also render to a self-contained HTML view ([`BenchReport::to_html`])
//! beside the JSON, for human reading without external assets.

use std::{fs, io, path::Path};

use serde::{Deserialize, Serialize};

use crate::schema::{BenchResult, SCHEMA_VERSION};

/// Description of the environment a report was produced in.
///
/// Covers the always-available build/target facts (crate version, OS, CPU
/// architecture) plus a best-effort hardware probe (CPU model, core counts, total
/// RAM) toward the benchmark-report template's required hardware block. The
/// hardware fields are `Option`: they are populated when [`Self::capture`] can read
/// them and omitted otherwise, so the metadata never claims more than it observed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunMetadata {
	/// Version of the `dsp-bench` crate that produced the report.
	pub dsp_bench_version: String,
	/// Target operating system (`std::env::consts::OS`, e.g. `windows`, `linux`).
	pub os: String,
	/// Target CPU architecture (`std::env::consts::ARCH`, e.g. `x86_64`).
	pub arch: String,
	/// CPU model / brand string, when it could be read (e.g.
	/// `AMD Ryzen 9 5900X 12-Core Processor`).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub cpu_model: Option<String>,
	/// Number of physical CPU cores, when available.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub cpu_cores_physical: Option<usize>,
	/// Number of logical CPUs (hardware threads), when available.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub cpu_cores_logical: Option<usize>,
	/// Total physical memory in bytes, when available.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub total_memory_bytes: Option<u64>,
	/// When the report was generated, RFC 3339 / ISO 8601. Empty when the caller
	/// constructs metadata without a timestamp (e.g. for reproducible tests).
	pub generated_at: String,
}

impl RunMetadata {
	/// Capture the current environment, stamping `generated_at` with `now`.
	///
	/// The build/target facts are always present; the hardware fields are a
	/// best-effort `sysinfo` probe — each is `Some` only when it read a usable
	/// value, so a platform that cannot report (say) a CPU brand simply omits it
	/// rather than recording a placeholder.
	///
	/// The timestamp is passed in rather than read from the clock so report
	/// construction stays deterministic and testable; callers that want wall
	/// time pass `chrono::Utc::now().to_rfc3339()`.
	#[must_use]
	pub fn capture(generated_at: String) -> Self {
		// Probe only CPU and memory — never enumerate processes (`new_all` is
		// heavy and, on some hosts, fragile). A single targeted refresh populates
		// the CPU list (brand/count) and total memory.
		let mut sys = sysinfo::System::new();
		sys.refresh_memory();
		sys.refresh_cpu_all();
		let cpu_model = sys.cpus().first().map(|c| c.brand().trim().to_string()).filter(|b| !b.is_empty());
		let cpu_cores_logical = Some(sys.cpus().len()).filter(|&n| n > 0);
		let cpu_cores_physical = sysinfo::System::physical_core_count();
		let total_memory_bytes = Some(sys.total_memory()).filter(|&b| b > 0);
		Self { dsp_bench_version: env!("CARGO_PKG_VERSION").to_string(), os: std::env::consts::OS.to_string(), arch: std::env::consts::ARCH.to_string(), cpu_model, cpu_cores_physical, cpu_cores_logical, total_memory_bytes, generated_at }
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
		let hw = hardware_meta_html(m);
		format!("<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<title>DSP-Bench report</title>\n<style>{style}</style>\n</head>\n<body>\n<h1>DSP-Bench report</h1>\n<p class=\"meta\">dsp-bench {version} \u{b7} {os}/{arch} \u{b7} schema v{schema} \u{b7} generated {generated} \u{b7} publishable: {publishable}</p>\n{hw}<table>\n<thead><tr>{head}</tr></thead>\n<tbody>\n{rows}</tbody>\n</table>\n</body>\n</html>\n", style = HTML_STYLE, schema = self.schema_version, head = HTML_HEAD_CELLS)
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
const HTML_HEAD_CELLS: &str = "<th>adapter</th><th>workload</th><th>shape</th><th>in</th><th>out</th><th>p50 ms</th><th>p95 ms</th><th>p99 ms</th><th>mean ms</th><th>pts/s</th><th>correct</th><th>rmse</th><th>mae</th><th>max</th><th>bias</th><th>enc</th><th>val B/pt</th><th>tot B/pt</th>";

/// Nanoseconds rendered as fractional milliseconds for display.
#[allow(clippy::cast_precision_loss)]
fn ms(ns: u64) -> f64 {
	ns as f64 / 1e6
}

/// Render the captured hardware facts as a second metadata paragraph, or an empty
/// string when none were captured (so the document degrades cleanly). Memory is
/// shown in GiB.
fn hardware_meta_html(m: &RunMetadata) -> String {
	let mut parts: Vec<String> = Vec::new();
	if let Some(cpu) = &m.cpu_model {
		parts.push(escape_html(cpu));
	}
	match (m.cpu_cores_physical, m.cpu_cores_logical) {
		(Some(p), Some(l)) => parts.push(format!("{p} physical / {l} logical cores")),
		(Some(p), None) => parts.push(format!("{p} physical cores")),
		(None, Some(l)) => parts.push(format!("{l} logical cores")),
		(None, None) => {}
	}
	if let Some(bytes) = m.total_memory_bytes {
		#[allow(clippy::cast_precision_loss)]
		let gib = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
		parts.push(format!("{gib:.1} GiB RAM"));
	}
	if parts.is_empty() {
		String::new()
	} else {
		format!("<p class=\"meta\">{}</p>\n", parts.join(" \u{b7} "))
	}
}

/// Render one result as an HTML table row. `is_best` tags the most-accurate row.
fn result_row_html(r: &BenchResult, is_best: bool) -> String {
	let l = &r.latency;
	let adapter = escape_html(&r.adapter);
	let workload = escape_html(&r.workload);
	let shape = r.dataset.signal_shape.map_or_else(|| "&mdash;".to_string(), |s| format!("{s:?}"));
	let correctness = if r.correctness.passed() { "PASS" } else { "FAIL" };
	// Accuracy is present only for a synthetic profile; otherwise the four cells
	// are em-dashes so the column stays aligned.
	let accuracy = r.accuracy.map_or_else(|| "<td>&mdash;</td><td>&mdash;</td><td>&mdash;</td><td>&mdash;</td>".to_string(), |a| format!("<td>{:.4}</td><td>{:.4}</td><td>{:.4}</td><td>{:+.4}</td>", a.rmse, a.mae, a.max_abs_error, a.bias));
	// Storage is present once `run_profile` estimates it; older artifacts and
	// callers that omit it render two em-dashes so the columns stay aligned. The
	// encoding name carries a lossy marker (`*`) so a non-exact pick is visible.
	let storage = r.storage.as_ref().map_or_else(|| "<td>&mdash;</td><td>&mdash;</td><td>&mdash;</td>".to_string(), |s| format!("<td>{}{}</td><td>{:.2}</td><td>{:.2}</td>", escape_html(&s.physical_type), if s.is_exact { "" } else { "*" }, s.bytes_per_point, s.total_bytes_per_point));
	let cls = if is_best { " class=\"best\"" } else { "" };
	format!("<tr{cls}><td>{adapter}</td><td>{workload}</td><td>{shape}</td><td>{in_pts}</td><td>{out_pts}</td><td>{p50:.3}</td><td>{p95:.3}</td><td>{p99:.3}</td><td>{mean:.3}</td><td>{tput:.0}</td><td>{correctness}</td>{accuracy}{storage}</tr>\n", in_pts = r.dataset.input_points, out_pts = r.dataset.output_points, p50 = ms(l.p50_ns), p95 = ms(l.p95_ns), p99 = ms(l.p99_ns), mean = ms(l.mean_ns), tput = r.throughput_points_per_sec)
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
		BenchResult { schema_version: SCHEMA_VERSION, profile: "interpolation-heavy-irregular".to_string(), adapter: adapter.to_string(), workload: "upsample_interpolate".to_string(), reps: 3, dataset: DatasetMeta { input_points: 200, output_points: 1000, irregular: true, missingness_fraction: 0.2, seed: 7, signal_shape: None }, latency: LatencyStats::from_samples(&[100, 200, 300]), latency_ci: None, throughput_points_per_sec: 5_000_000.0, timing: crate::schema::TimingBreakdown { dataset_generation_ns: 5_000, measured_ns: 600, end_to_end_ns: 6_200 }, correctness: CorrectnessReport { output_count_ok: publishable, expected_output_points: 1000, actual_output_points: if publishable { 1000 } else { 0 }, values_finite: publishable }, accuracy: None, storage: None }
	}

	/// A publishable result for `adapter` carrying an accuracy report with the given
	/// RMSE (the other accuracy fields are irrelevant to `most_accurate`).
	fn result_with_rmse(adapter: &str, rmse: f64) -> BenchResult {
		let mut r = sample_result(adapter, true);
		r.accuracy = Some(crate::accuracy::AccuracyMetrics { count: 1000, rmse, mae: rmse, max_abs_error: rmse, bias: 0.0 });
		r
	}

	fn metadata() -> RunMetadata {
		RunMetadata { dsp_bench_version: "0.1.0".to_string(), os: "testos".to_string(), arch: "testarch".to_string(), cpu_model: Some("Test CPU 9000".to_string()), cpu_cores_physical: Some(8), cpu_cores_logical: Some(16), total_memory_bytes: Some(34_359_738_368), generated_at: "2026-06-06T00:00:00+00:00".to_string() }
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
	fn capture_probes_hardware_consistently() {
		// The hardware probe is best-effort, but whatever it reports must be
		// self-consistent: any reported count is positive, logical cores are at
		// least physical cores, and a reported CPU model is non-empty. On a real
		// host the runner should read at least the logical core count and RAM.
		let meta = RunMetadata::capture(String::new());
		if let Some(model) = &meta.cpu_model {
			assert!(!model.is_empty(), "a reported CPU model must be non-empty");
		}
		if let Some(p) = meta.cpu_cores_physical {
			assert!(p > 0, "physical core count must be positive when reported");
		}
		if let Some(l) = meta.cpu_cores_logical {
			assert!(l > 0, "logical core count must be positive when reported");
		}
		if let (Some(p), Some(l)) = (meta.cpu_cores_physical, meta.cpu_cores_logical) {
			assert!(l >= p, "logical cores ({l}) must be >= physical cores ({p})");
		}
		if let Some(bytes) = meta.total_memory_bytes {
			assert!(bytes > 0, "reported total memory must be positive");
		}
		assert!(meta.cpu_cores_logical.is_some(), "a host should report a logical core count");
		assert!(meta.total_memory_bytes.is_some(), "a host should report total memory");
	}

	#[test]
	fn to_html_surfaces_captured_hardware() {
		// The hardware metadata renders as a second meta paragraph with the CPU,
		// core split, and RAM (in GiB). 34359738368 bytes == 32.0 GiB.
		let report = BenchReport::with_results(metadata(), vec![sample_result("dsp", true)]);
		let html = report.to_html();
		assert!(html.contains("Test CPU 9000"), "CPU model must appear: {html}");
		assert!(html.contains("8 physical / 16 logical cores"), "core split must appear: {html}");
		assert!(html.contains("32.0 GiB RAM"), "RAM in GiB must appear: {html}");
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
	fn to_html_renders_storage_columns_when_present_and_dashes_when_absent() {
		// A result carrying a storage estimate must render its encoding name and
		// bytes/point; a result without one (the sample default) shows em-dashes so
		// the columns stay aligned. The header always carries the two storage cells.
		let mut with_storage = sample_result("dsp", true);
		with_storage.storage = Some(crate::schema::StorageEstimate { physical_type: "scaled_i64".to_string(), value_count: 200, estimated_value_bytes: 1600, realized_value_bytes: 420, value_codec: "varint".to_string(), bytes_per_point: 2.1, is_exact: true, lossy_count: 0, max_abs_error: "0".to_string(), tolerance: "0".to_string(), timestamp_unit: "micros".to_string(), timestamp_encoding: "delta_of_delta".to_string(), timestamp_bytes: 208, timestamp_bytes_per_point: 1.04, total_bytes_per_point: 3.14, advisory_best_f64_bytes: None, advisory_best_f64_codec: None, advisory_fire_timestamp_bytes: Some(190), advisory_delta_cascade_value_bytes: None });
		let report = BenchReport::with_results(metadata(), vec![with_storage, sample_result("baseline-linear", true)]);
		let html = report.to_html();
		assert!(html.contains("<th>enc</th><th>val B/pt</th><th>tot B/pt</th>"), "header must carry storage columns: {html}");
		// Value bytes/point then total (value + timestamp) bytes/point — the realized
		// (v10 headline) figures.
		assert!(html.contains("<td>scaled_i64</td><td>2.10</td><td>3.14</td>"), "storage cells must render: {html}");
		// The storage-less row keeps the columns aligned with em-dashes.
		assert!(html.contains("<td>&mdash;</td><td>&mdash;</td><td>&mdash;</td></tr>"), "absent storage must render dashes: {html}");
	}

	#[test]
	fn to_html_renders_the_workload_column_per_row() {
		// With three workload types now sharing the report schema, the HTML table must
		// name each result's workload so a point-lookup report is distinguishable from
		// an interpolate one at a glance. The header carries the column and each row
		// renders its own workload.
		let mut point_lookup = sample_result("dsp", true);
		point_lookup.workload = "point_lookup".to_string();
		let report = BenchReport::with_results(metadata(), vec![point_lookup]);
		let html = report.to_html();
		assert!(html.contains("<th>adapter</th><th>workload</th>"), "header must carry the workload column: {html}");
		assert!(html.contains("<td>dsp</td><td>point_lookup</td>"), "row must render its workload: {html}");
	}

	#[test]
	fn to_html_marks_a_lossy_storage_encoding() {
		// A non-exact encoding pick is flagged with a trailing `*` so a lossy storage
		// choice is visible at a glance in the table.
		let mut r = sample_result("dsp", true);
		r.storage = Some(crate::schema::StorageEstimate { physical_type: "f64".to_string(), value_count: 10, estimated_value_bytes: 80, realized_value_bytes: 80, value_codec: "varint".to_string(), bytes_per_point: 8.0, is_exact: false, lossy_count: 3, max_abs_error: "0.0001".to_string(), tolerance: "0.001".to_string(), timestamp_unit: "micros".to_string(), timestamp_encoding: "delta_of_delta".to_string(), timestamp_bytes: 12, timestamp_bytes_per_point: 1.2, total_bytes_per_point: 9.2, advisory_best_f64_bytes: Some(64), advisory_best_f64_codec: Some("gorilla".to_string()), advisory_fire_timestamp_bytes: Some(10), advisory_delta_cascade_value_bytes: None });
		let html = BenchReport::with_results(metadata(), vec![r]).to_html();
		assert!(html.contains("<td>f64*</td><td>8.00</td><td>9.20</td>"), "lossy encoding must be flagged: {html}");
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
