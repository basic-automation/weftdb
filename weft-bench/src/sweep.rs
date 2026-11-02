//! Saturation sweeps (fair protocol 1.1, #147).
//!
//! One run at one setting says how fast the engine was there, not where it stops getting faster.
//! `weft-bench sweep KNOB=v1,v2,… -- RUN ARGS` runs the same benchmark once per value — the knob
//! is any run flag (`--ingest-batch`, `--ingest-online`, `--points`, `--sp-series`, …) or
//! `threads`, the worker pool's size (`RAYON_NUM_THREADS`) — each into its own directory with its
//! own report and manifest, then lays every result out as a curve over the knob.
//!
//! The summary names each curve's **knee**: the first value past which a step up the knob buys
//! less than [`KNEE_GAIN`] more throughput. A curve whose every step still gains has no knee in
//! the swept range, which is a finding too: sweep further. A step whose result failed its gate is
//! shown and never chosen as a knee.

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use crate::{report::BenchReport, schema::BenchResult};

/// A throughput gain below this fraction per step up the knob counts as saturated.
pub const KNEE_GAIN: f64 = 0.10;

/// The knob that sets the worker pool's size through the environment rather than a flag.
pub const THREADS_KNOB: &str = "threads";

/// One result at one knob value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SweepPoint {
	/// The knob's value for this run.
	pub value: String,
	/// Median latency.
	pub p50_ns: u64,
	/// 99th-percentile latency.
	pub p99_ns: u64,
	/// Throughput, in the result's own unit (points or rows per second).
	pub throughput: f64,
	/// The result passed its gate.
	pub passed: bool,
}

/// One result's curve over the knob.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SweepCurve {
	/// The result's profile.
	pub profile: String,
	/// The result's workload.
	pub workload: String,
	/// One point per knob value that produced this result, in sweep order.
	pub points: Vec<SweepPoint>,
	/// The value past which throughput stopped gaining [`KNEE_GAIN`] per step; `None` if it never did.
	pub knee: Option<String>,
}

/// A whole sweep.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sweep {
	/// The knob swept (`--ingest-batch`, `threads`, …).
	pub knob: String,
	/// The values, in order.
	pub values: Vec<String>,
	/// The run's own arguments, the knob aside.
	pub args: Vec<String>,
	/// One curve per result the runs produced.
	pub curves: Vec<SweepCurve>,
}

impl Sweep {
	/// Lay the reports each value produced out as curves.
	#[must_use]
	pub fn from_runs(knob: &str, args: &[String], runs: &[(String, Vec<BenchReport>)]) -> Self {
		let mut curves: Vec<SweepCurve> = Vec::new();
		for (value, reports) in runs {
			for result in reports.iter().flat_map(|r| &r.results) {
				let point = point(value, result);
				match curves.iter_mut().find(|c| c.profile == result.profile && c.workload == result.workload) {
					Some(curve) => curve.points.push(point),
					None => curves.push(SweepCurve { profile: result.profile.clone(), workload: result.workload.clone(), points: vec![point], knee: None }),
				}
			}
		}
		for curve in &mut curves {
			curve.knee = knee(&curve.points);
		}
		Self { knob: knob.to_string(), values: runs.iter().map(|(v, _)| v.clone()).collect(), args: args.to_vec(), curves }
	}

	/// True when every point of every curve passed its gate.
	#[must_use]
	pub fn all_passed(&self) -> bool {
		self.curves.iter().flat_map(|c| &c.points).all(|p| p.passed)
	}

	/// The sweep as Markdown: one table per curve, its knee beneath.
	#[must_use]
	pub fn to_markdown(&self) -> String {
		let mut out = format!("# Sweep over `{}`\n\nValues: {}. Run: `weft-bench {}`.\n", self.knob, self.values.join(", "), self.args.join(" "));
		for curve in &self.curves {
			let _ = write!(out, "\n## {} ({})\n\n| {} | p50 ms | p99 ms | throughput /s | gate |\n|---:|---:|---:|---:|---|\n", curve.profile, curve.workload, self.knob);
			for p in &curve.points {
				#[allow(clippy::cast_precision_loss)]
				let ms = |ns: u64| ns as f64 / 1e6;
				let _ = writeln!(out, "| {} | {:.3} | {:.3} | {:.0} | {} |", p.value, ms(p.p50_ns), ms(p.p99_ns), p.throughput, if p.passed { "PASS" } else { "FAIL" });
			}
			let _ = writeln!(out, "\n{}", curve.knee.as_ref().map_or_else(|| format!("No knee: every step gained at least {:.0}% throughput, so the curve has not saturated in this range.", KNEE_GAIN * 100.0), |k| format!("Knee at **{k}**: the next step gained less than {:.0}% throughput.", KNEE_GAIN * 100.0)));
		}
		out
	}
}

fn point(value: &str, result: &BenchResult) -> SweepPoint {
	SweepPoint { value: value.to_string(), p50_ns: result.latency.p50_ns, p99_ns: result.latency.p99_ns, throughput: result.throughput_points_per_sec, passed: result.correctness.passed() }
}

/// The first passing point whose next passing point gains less than [`KNEE_GAIN`] throughput.
fn knee(points: &[SweepPoint]) -> Option<String> {
	let passing: Vec<&SweepPoint> = points.iter().filter(|p| p.passed).collect();
	passing.windows(2).find(|w| w[0].throughput > 0.0 && w[1].throughput < w[0].throughput * (1.0 + KNEE_GAIN)).map(|w| w[0].value.clone())
}

/// Split `KNOB=v1,v2,…` into the knob and its values.
///
/// # Errors
///
/// A spec without `=`, an empty knob, a knob that is neither a flag nor [`THREADS_KNOB`], or
/// fewer than two values.
pub fn parse_spec(spec: &str) -> Result<(String, Vec<String>), String> {
	let (knob, values) = spec.split_once('=').ok_or_else(|| format!("sweep {spec:?}: expected KNOB=v1,v2,…"))?;
	if !(knob.starts_with("--") && knob.len() > 2 || knob == THREADS_KNOB) {
		return Err(format!("sweep knob {knob:?}: a run flag such as --ingest-batch, or {THREADS_KNOB}"));
	}
	if knob == "--out-dir" {
		return Err("sweep cannot vary --out-dir: each value gets its own".to_string());
	}
	let values: Vec<String> = values.split(',').map(str::trim).filter(|v| !v.is_empty()).map(str::to_string).collect();
	if values.len() < 2 {
		return Err(format!("sweep {knob}: a curve needs at least two values, got {}", values.len()));
	}
	Ok((knob.to_string(), values))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn p(value: &str, throughput: f64, passed: bool) -> SweepPoint {
		SweepPoint { value: value.into(), p50_ns: 1, p99_ns: 2, throughput, passed }
	}

	#[test]
	fn the_knee_is_where_a_step_stops_paying() {
		assert_eq!(knee(&[p("1", 100.0, true), p("2", 190.0, true), p("4", 200.0, true), p("8", 205.0, true)]), Some("2".into()));
		assert_eq!(knee(&[p("1", 100.0, true), p("2", 150.0, true), p("4", 300.0, true)]), None, "still climbing");
		// A failed step is never the knee, and the curve continues past it.
		assert_eq!(knee(&[p("1", 100.0, true), p("2", 101.0, false), p("4", 300.0, true), p("8", 310.0, true)]), Some("4".into()));
	}

	#[test]
	fn a_spec_is_a_flag_or_threads_with_two_values_or_more() {
		assert_eq!(parse_spec("--ingest-batch=1k,64k").expect("parses"), ("--ingest-batch".to_string(), vec!["1k".to_string(), "64k".to_string()]));
		assert_eq!(parse_spec("threads=1, 2,4").expect("parses").1, ["1", "2", "4"]);
		for bad in ["--ingest-batch", "batch=1,2", "--=1,2", "--points=5", "--out-dir=a,b"] {
			assert!(parse_spec(bad).is_err(), "{bad}");
		}
	}

	#[test]
	fn runs_become_one_curve_per_result() {
		use crate::{report::RunMetadata, schema::*, stats::LatencyStats};
		let result = |profile: &str, throughput: f64| BenchResult { schema_version: SCHEMA_VERSION, profile: profile.into(), adapter: "weftdb".into(), workload: "ingest".into(), reps: 1, dataset: DatasetMeta { input_points: 1, output_points: 1, irregular: false, missingness_fraction: 0.0, seed: 1, signal_shape: None }, latency: LatencyStats::from_samples(&[10]), latency_ci: None, throughput_points_per_sec: throughput, timing: TimingBreakdown { dataset_generation_ns: 0, measured_ns: 10, end_to_end_ns: 10 }, correctness: CorrectnessReport { output_count_ok: true, expected_output_points: 1, actual_output_points: 1, values_finite: true, result_sha256: None }, accuracy: None, storage: None, cold_warm: None, gpu_pool: None, adapter_info: None, growth: None, online: None, compressed_query: None, analytics: None, samples_ns: vec![10], between_run: None };
		let report = |results| BenchReport::with_results(RunMetadata::capture("2026-11-02T00:00:00Z".into()), results);
		let runs = vec![("1k".to_string(), vec![report(vec![result("a", 100.0), result("b", 10.0)])]), ("64k".to_string(), vec![report(vec![result("a", 105.0), result("b", 50.0)])])];
		let sweep = Sweep::from_runs("--ingest-batch", &["--ingest".into()], &runs);
		assert_eq!(sweep.curves.len(), 2);
		assert_eq!(sweep.curves[0].knee.as_deref(), Some("1k"));
		assert_eq!(sweep.curves[1].knee, None);
		let md = sweep.to_markdown();
		assert!(md.contains("Knee at **1k**") && md.contains("has not saturated"), "{md}");
		assert!(sweep.all_passed());
	}
}
