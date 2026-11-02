//! Report surfaces beyond JSON, HTML and Parquet (roadmap 1, #149).
//!
//! A report is read in more places than a browser: a pull request or a job summary takes Markdown,
//! a spreadsheet or `awk` takes CSV, and a dashboard takes metrics. [`to_markdown`], [`to_csv`]
//! and [`to_prometheus`] render the same results three ways, one line or row per result, each
//! carrying the gate beside the timing so no surface shows a number without saying whether it
//! counts. `weft-bench export REPORT.json --format md|csv|prom` converts a stored report (a
//! combined one included); a run writes them beside its JSON with `--markdown-report`, `--csv` and
//! `--prom`.
//!
//! The Prometheus text is the exposition format a node-exporter textfile collector or a Pushgateway
//! reads, so a nightly run lands in the Grafana dashboard under `bench/grafana/`. It carries no
//! timestamps (the collector stamps the scrape) and labels each series with the result's profile,
//! workload and adapter.

use std::fmt::Write as _;

use crate::{report::BenchReport, schema::BenchResult};

/// The surfaces [`render`] writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
	/// A Markdown table.
	Markdown,
	/// Comma-separated values with a header row.
	Csv,
	/// Prometheus text exposition.
	Prometheus,
}

impl Format {
	/// Parse a CLI name (`md`, `markdown`, `csv`, `prom`, `prometheus`).
	#[must_use]
	pub fn parse(s: &str) -> Option<Self> {
		match s {
			"md" | "markdown" => Some(Self::Markdown),
			"csv" => Some(Self::Csv),
			"prom" | "prometheus" => Some(Self::Prometheus),
			_ => None,
		}
	}

	/// The file extension the surface is written with.
	#[must_use]
	pub const fn extension(self) -> &'static str {
		match self {
			Self::Markdown => "md",
			Self::Csv => "csv",
			Self::Prometheus => "prom",
		}
	}
}

/// `report` in `format`.
#[must_use]
pub fn render(report: &BenchReport, format: Format) -> String {
	match format {
		Format::Markdown => to_markdown(report),
		Format::Csv => to_csv(report),
		Format::Prometheus => to_prometheus(report),
	}
}

#[allow(clippy::cast_precision_loss)]
fn ms(ns: u64) -> f64 {
	ns as f64 / 1e6
}

fn gate(r: &BenchResult) -> &'static str {
	if r.is_publishable() {
		"PASS"
	} else {
		"FAIL"
	}
}

/// The report as a Markdown table, with the run's machine and time above it.
#[must_use]
pub fn to_markdown(report: &BenchReport) -> String {
	let m = &report.metadata;
	let mut out = format!("# weft-bench report\n\nGenerated {} by weft-bench {} on {} ({} {}).\n\n", m.generated_at, m.weft_bench_version, m.cpu_model.as_deref().unwrap_or("an unreported CPU"), m.os, m.arch);
	out.push_str("| profile | workload | adapter | reps | p50 ms | p95 ms | p99 ms | throughput /s | gate | answer sha256 |\n|---|---|---|---:|---:|---:|---:|---:|---|---|\n");
	for r in &report.results {
		let digest = r.correctness.result_sha256.as_deref().map_or_else(|| "—".to_string(), |d| format!("`{}`", &d[..d.len().min(12)]));
		let _ = writeln!(out, "| {} | {} | {} | {} | {:.3} | {:.3} | {:.3} | {:.0} | {} | {digest} |", cell(&r.profile), cell(&r.workload), cell(&r.adapter), r.reps, ms(r.latency.p50_ns), ms(r.latency.p95_ns), ms(r.latency.p99_ns), r.throughput_points_per_sec, gate(r));
	}
	out
}

fn cell(s: &str) -> String {
	s.replace('|', "\\|")
}

/// The report as CSV: a header row, then one row per result.
#[must_use]
pub fn to_csv(report: &BenchReport) -> String {
	let mut out = String::from("generated_at,profile,workload,adapter,reps,input_points,output_points,seed,p50_ns,p95_ns,p99_ns,mean_ns,stddev_ns,throughput_per_sec,publishable,result_sha256\n");
	for r in &report.results {
		let _ = writeln!(out, "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}", csv(&report.metadata.generated_at), csv(&r.profile), csv(&r.workload), csv(&r.adapter), r.reps, r.dataset.input_points, r.dataset.output_points, r.dataset.seed, r.latency.p50_ns, r.latency.p95_ns, r.latency.p99_ns, r.latency.mean_ns, r.latency.stddev_ns, r.throughput_points_per_sec, r.is_publishable(), r.correctness.result_sha256.as_deref().unwrap_or(""));
	}
	out
}

/// A CSV field, quoted when it holds a comma, quote or line break.
fn csv(s: &str) -> String {
	if s.contains([',', '"', '\n', '\r']) {
		format!("\"{}\"", s.replace('"', "\"\""))
	} else {
		s.to_string()
	}
}

/// The report as Prometheus text exposition: latency quantiles in seconds, throughput, reps and the
/// gate (1 publishable, 0 not) per result, and one `weft_bench_run_info` series naming the run.
#[must_use]
pub fn to_prometheus(report: &BenchReport) -> String {
	let mut out = String::new();
	let m = &report.metadata;
	let _ = writeln!(out, "# HELP weft_bench_run_info The run that produced these results.\n# TYPE weft_bench_run_info gauge\nweft_bench_run_info{{version=\"{}\",generated_at=\"{}\",cpu=\"{}\"}} 1", label(&m.weft_bench_version), label(&m.generated_at), label(m.cpu_model.as_deref().unwrap_or("unknown")));
	let series: [(&str, &str, &str); 4] = [("weft_bench_latency_seconds", "gauge", "Latency of one rep, by quantile."), ("weft_bench_throughput_per_second", "gauge", "Points or rows per second of measured time."), ("weft_bench_reps", "gauge", "Timed reps behind the figures."), ("weft_bench_publishable", "gauge", "1 when the result passed its correctness gate and may be published, else 0.")];
	for (name, kind, help) in series {
		let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
		for r in &report.results {
			let labels = format!("profile=\"{}\",workload=\"{}\",adapter=\"{}\"", label(&r.profile), label(&r.workload), label(&r.adapter));
			match name {
				"weft_bench_latency_seconds" => {
					for (q, ns) in [("0.5", r.latency.p50_ns), ("0.95", r.latency.p95_ns), ("0.99", r.latency.p99_ns)] {
						let _ = writeln!(out, "{name}{{{labels},quantile=\"{q}\"}} {}", ms(ns) / 1e3);
					}
				}
				"weft_bench_throughput_per_second" => {
					let _ = writeln!(out, "{name}{{{labels}}} {}", r.throughput_points_per_sec);
				}
				"weft_bench_reps" => {
					let _ = writeln!(out, "{name}{{{labels}}} {}", r.reps);
				}
				_ => {
					let _ = writeln!(out, "{name}{{{labels}}} {}", u8::from(r.is_publishable()));
				}
			}
		}
	}
	out
}

/// A label value escaped as the exposition format requires.
fn label(s: &str) -> String {
	s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		report::RunMetadata, schema::{CorrectnessReport, DatasetMeta, TimingBreakdown, SCHEMA_VERSION}, stats::LatencyStats
	};

	fn report() -> BenchReport {
		let result = |profile: &str, ok: bool| BenchResult { schema_version: SCHEMA_VERSION, profile: profile.into(), adapter: "weftdb".into(), workload: "ingest".into(), reps: 3, dataset: DatasetMeta { input_points: 10, output_points: 10, irregular: false, missingness_fraction: 0.0, seed: 7, signal_shape: None }, latency: LatencyStats::from_samples(&[1_000_000, 2_000_000, 3_000_000]), latency_ci: None, throughput_points_per_sec: 5.0, timing: TimingBreakdown { dataset_generation_ns: 0, measured_ns: 6, end_to_end_ns: 6 }, correctness: CorrectnessReport { output_count_ok: ok, expected_output_points: 10, actual_output_points: 10, values_finite: true, result_sha256: Some("abcdef0123456789".into()) }, accuracy: None, storage: None, cold_warm: None, gpu_pool: None, adapter_info: None, growth: None, online: None, compressed_query: None, analytics: None, samples_ns: vec![], between_run: None };
		BenchReport::with_results(RunMetadata::capture("2026-11-02T00:00:00Z".into()), vec![result("a|b", true), result("c,\"d\"", false)])
	}

	#[test]
	fn markdown_escapes_pipes_and_shows_the_gate() {
		let md = to_markdown(&report());
		assert!(md.contains("| a\\|b | ingest | weftdb | 3 | 2.000 |"), "{md}");
		assert!(md.contains("| PASS | `abcdef012345` |") && md.contains("| FAIL |"), "{md}");
	}

	#[test]
	fn csv_quotes_what_needs_it_and_has_one_row_per_result() {
		let csv = to_csv(&report());
		let lines: Vec<&str> = csv.lines().collect();
		assert_eq!(lines.len(), 3);
		assert!(lines[2].contains(",\"c,\"\"d\"\"\",ingest,"), "{}", lines[2]);
		assert!(lines[1].ends_with(",true,abcdef0123456789") && lines[2].ends_with(",false,abcdef0123456789"));
	}

	#[test]
	fn prometheus_text_has_a_family_per_measure_and_escaped_labels() {
		let prom = to_prometheus(&report());
		assert!(prom.contains("weft_bench_latency_seconds{profile=\"a|b\",workload=\"ingest\",adapter=\"weftdb\",quantile=\"0.5\"} 0.002"), "{prom}");
		assert!(prom.contains("weft_bench_publishable{profile=\"c,\\\"d\\\"\",workload=\"ingest\",adapter=\"weftdb\"} 0"), "{prom}");
		assert_eq!(prom.matches("# TYPE ").count(), 5);
		for f in [Format::Markdown, Format::Csv, Format::Prometheus] {
			assert_eq!(Format::parse(f.extension()), Some(f));
		}
	}
}
