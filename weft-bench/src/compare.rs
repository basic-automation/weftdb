//! Decide whether one saved benchmark run is slower than another (1.0 plan EVI-5).
//!
//! A nightly job needs a verdict a machine can act on and a human can argue with. The naive rule
//! — candidate p99 exceeds baseline p99 by N percent, fail the build — fires on every run on a
//! shared runner, and a check that cries wolf is a check that gets ignored, so this module gates a
//! regression twice:
//!
//! 1. the move must exceed a relative threshold (ten percent by default), and
//! 2. the two runs' bootstrap intervals ([`BenchResult::latency_ci`]) must not overlap.
//!
//! A move past the threshold whose intervals *do* overlap is reported as
//! [`Verdict::Suspicious`]: visible in the summary, not a build failure. A metric with no recorded
//! interval can reach `Suspicious` but never `Regression` — without an interval there is no
//! evidence the move is bigger than the noise, and pretending otherwise would be exactly the kind
//! of unsupported claim the 1.0 evidence plan exists to remove.
//!
//! Provenance is checked before any number is compared. Two runs from different operating systems
//! or architectures are [`CompareError::Incomparable`] rather than a comparison with a caveat,
//! because the delta would measure the platform and not the code. Weaker differences (CPU model,
//! core count, engine version) are allowed and listed as notes on the result, since a nightly on
//! hosted runners changes machine every night and refusing on that would never compare anything.

use std::{
	collections::BTreeMap, fmt, path::{Path, PathBuf}
};

use serde::{Deserialize, Serialize};

// `MetricMove`, `WorkloadVerdict` and `Comparison` serialize only: a metric name is a
// `&'static str`, and deserializing one would demand the input outlive `'static`. A verdict
// file is read back as `serde_json::Value`, which needs no borrowed types.
use crate::manifest::{Better, Measure};
use crate::{
	report::BenchReport, schema::{BenchResult, DatasetMeta}, stats::ConfidenceInterval
};

/// Percentage by which a metric must worsen before the comparison calls it anything at all.
pub const DEFAULT_THRESHOLD_PCT: f64 = 10.0;

/// What one metric did between the baseline run and the candidate run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
	/// The candidate is not worse by more than the threshold.
	Flat,
	/// The candidate is better by more than the threshold.
	Improved,
	/// Worse by more than the threshold, but the two runs' intervals overlap, so the move is not
	/// separated from the noise. Reported, not gating.
	Suspicious,
	/// Worse by more than the threshold with intervals that do not overlap. This is what fails a
	/// nightly build.
	Regression,
}

impl Verdict {
	/// The word used in the table and in the JSON verdict.
	#[must_use]
	pub const fn label(self) -> &'static str {
		match self {
			Self::Flat => "flat",
			Self::Improved => "improved",
			Self::Suspicious => "suspicious",
			Self::Regression => "regression",
		}
	}

	/// True when the nightly build should stop.
	#[must_use]
	pub const fn is_regression(self) -> bool {
		matches!(self, Self::Regression)
	}

	/// True when the move is worse than the baseline, gating or not.
	#[must_use]
	pub const fn is_worse(self) -> bool {
		matches!(self, Self::Suspicious | Self::Regression)
	}

	/// Ordering used to roll metric verdicts up into one workload verdict: the worst metric decides.
	const fn rank(self) -> u8 {
		match self {
			Self::Flat => 0,
			Self::Improved => 1,
			Self::Suspicious => 2,
			Self::Regression => 3,
		}
	}
}

impl fmt::Display for Verdict {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(self.label())
	}
}

/// How the two runs' bootstrap intervals related for one latency metric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoiseGate {
	/// Both runs recorded an interval and they did not overlap: the move is separated from noise.
	Separated,
	/// Both runs recorded an intervals and they overlapped.
	Overlap,
	/// At least one side has no interval, so no claim about noise can be made either way.
	NotMeasured,
	/// The measure is a flag rather than a magnitude, so there is no noise to consider: a pass that
	/// became a fail is certain, and the threshold does not get a vote on it.
	Certain,
}

impl NoiseGate {
	/// Whether this gate lets a beyond-threshold move become a [`Verdict::Regression`].
	const fn allows_regression(self) -> bool {
		matches!(self, Self::Separated | Self::Certain)
	}
}

/// One measured number, in both runs, with the move between them.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MetricMove {
	/// The measure compared, named exactly as the manifest's `grade` block names it, e.g.
	/// `latency.p95_ns`.
	pub metric: String,
	/// `true` when a larger number is the better result (throughput), `false` for latencies.
	pub higher_is_better: bool,
	/// The baseline value.
	pub baseline: f64,
	/// The candidate value.
	pub candidate: f64,
	/// Signed percentage change of the candidate against the baseline; positive means larger.
	pub change_pct: f64,
	/// How much worse the candidate is, as a positive percentage, or 0 when it is not worse.
	pub worse_by_pct: f64,
	/// How the two runs' intervals related.
	pub noise: NoiseGate,
	/// The verdict for this one metric.
	pub verdict: Verdict,
}

/// The identity of a measured workload: two results are the same workload only when every field
/// matches, so a candidate that changed its dataset shape simply does not pair up.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct WorkloadKey {
	/// The profile name the run recorded.
	pub profile: String,
	/// The adapter under test.
	pub adapter: String,
	/// The workload label.
	pub workload: String,
	/// The dataset shape, serialised so an absent signal shape and an unknown one differ.
	pub shape: String,
}

impl fmt::Display for WorkloadKey {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{} / {} / {} [{}]", self.profile, self.adapter, self.workload, self.shape)
	}
}

/// Every metric compared for one workload that appeared in both runs.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WorkloadVerdict {
	/// What this workload was.
	pub key: WorkloadKey,
	/// The metrics compared, in a fixed order.
	pub metrics: Vec<MetricMove>,
	/// The worst metric verdict: one regressed metric makes the workload regressed.
	pub verdict: Verdict,
}

/// The whole comparison.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Comparison {
	/// The threshold that was in force, as a percentage.
	pub threshold_pct: f64,
	/// Provenance differences that were tolerated; empty when the two runs match closely.
	pub notes: Vec<String>,
	/// Workloads present in both runs, ordered by key.
	pub workloads: Vec<WorkloadVerdict>,
	/// Workloads only the baseline ran, which the candidate therefore did not measure.
	pub only_in_baseline: Vec<WorkloadKey>,
	/// Workloads only the candidate ran, which nothing can be said about yet.
	pub only_in_candidate: Vec<WorkloadKey>,
}

impl Comparison {
	/// The workloads that regressed, and would fail a nightly build.
	#[must_use]
	pub fn regressions(&self) -> Vec<&WorkloadVerdict> {
		self.workloads.iter().filter(|w| w.verdict.is_regression()).collect()
	}

	/// The workloads that moved beyond the threshold without separating from the noise.
	#[must_use]
	pub fn suspicious(&self) -> Vec<&WorkloadVerdict> {
		self.workloads.iter().filter(|w| w.verdict == Verdict::Suspicious).collect()
	}

	/// The workloads that improved beyond the threshold.
	#[must_use]
	pub fn improvements(&self) -> Vec<&WorkloadVerdict> {
		self.workloads.iter().filter(|w| w.verdict == Verdict::Improved).collect()
	}

	/// Whether the comparison should stop a build. `strict` also stops it on
	/// [`Verdict::Suspicious`], which is what a maintainer running it locally wants; a nightly
	/// leaves it off so that runner noise cannot fail the branch.
	#[must_use]
	pub fn failed(&self, strict: bool) -> bool {
		// Nothing paired means nothing was measured. A nightly that compared no workloads and
		// reported green is worse than one that regressed, because it looks like a pass: the
		// usual causes are a renamed workload, a changed dataset shape or a changed seed.
		if self.workloads.is_empty() {
			return true;
		}
		if strict {
			self.workloads.iter().any(|w| w.verdict.is_worse())
		} else {
			self.workloads.iter().any(|w| w.verdict.is_regression())
		}
	}

	/// A short one-line tally, suitable as a commit status or a job summary heading.
	#[must_use]
	pub fn tally(&self) -> String {
		format!("{} compared, {} regressed, {} suspicious, {} improved, {} unpaired", self.workloads.len(), self.regressions().len(), self.suspicious().len(), self.improvements().len(), self.only_in_baseline.len() + self.only_in_candidate.len())
	}

	/// The comparison as a markdown table, for a job summary.
	#[must_use]
	pub fn to_markdown(&self) -> String {
		use std::fmt::Write as _;
		let mut out = String::new();
		let _ = writeln!(out, "## Benchmark comparison\n\nThreshold {}% · `{}`\n", format_pct(self.threshold_pct), self.tally());
		if !self.notes.is_empty() {
			let _ = writeln!(out, "Provenance differences tolerated:");
			for note in &self.notes {
				let _ = writeln!(out, "- {note}");
			}
			let _ = writeln!(out);
		}
		if self.workloads.is_empty() {
			let _ = writeln!(out, "_No workload appears in both runs, so nothing was compared._");
		} else {
			let _ = writeln!(out, "| workload | metric | baseline | candidate | change | noise | verdict |");
			let _ = writeln!(out, "| --- | --- | ---: | ---: | ---: | --- | --- |");
			for workload in &self.workloads {
				for metric in &workload.metrics {
					let _ = writeln!(out, "| {} | {} | {:.0} | {:.0} | {:+.1}% | {} | **{}** |", workload.key, metric.metric, metric.baseline, metric.candidate, metric.change_pct, noise_label(metric.noise), metric.verdict.label());
				}
			}
		}
		if !self.only_in_baseline.is_empty() || !self.only_in_candidate.is_empty() {
			let _ = writeln!(out, "\nUnpaired workloads are not evidence of anything: the baseline ran `{}` that the candidate did not, and the candidate ran `{}` that the baseline did not.", self.only_in_baseline.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "), self.only_in_candidate.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "));
		}
		out
	}

	/// The comparison as the JSON verdict artifact a nightly job stores.
	///
	/// # Errors
	///
	/// Returns the [`serde_json`] error when the structure cannot be serialised.
	pub fn to_json(&self) -> Result<String, serde_json::Error> {
		serde_json::to_string_pretty(self)
	}
}

/// The word a table or a job summary uses for one noise gate.
#[must_use]
pub const fn noise_label(gate: NoiseGate) -> &'static str {
	match gate {
		NoiseGate::Separated => "separated",
		NoiseGate::Overlap => "overlapping",
		NoiseGate::NotMeasured => "not measured",
		NoiseGate::Certain => "certain",
	}
}

/// Render a percentage without a trailing `.0`, so a 10% threshold reads as `10%` and a 7.5%
/// one as `7.5%`.
#[must_use]
pub fn format_pct(value: f64) -> String {
	if (value - value.round()).abs() < f64::EPSILON {
		format!("{:.0}", value)
	} else {
		format!("{value:.1}")
	}
}

/// Why a comparison could not be made, or could not be trusted.
#[derive(Debug, Clone, PartialEq)]
pub enum CompareError {
	/// A report could not be read.
	Io {
		/// The path that failed.
		path: PathBuf,
		/// The OS message.
		reason: String,
	},
	/// A report was not readable as a v22 benchmark report.
	Parse {
		/// The path that failed.
		path: PathBuf,
		/// The serde message.
		reason: String,
	},
	/// The two runs measured different platforms, so no number from one means anything about the
	/// other. Pass `ignore_provenance` to compare anyway.
	Incomparable(Vec<String>),
}

impl fmt::Display for CompareError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Io { path, reason } => write!(f, "cannot read the benchmark report at {}: {reason}", path.display()),
			Self::Parse { path, reason } => write!(f, "cannot parse a benchmark report at {}: {reason}", path.display()),
			Self::Incomparable(reasons) => write!(f, "the two runs are not comparable: {}", reasons.join("; ")),
		}
	}
}

impl std::error::Error for CompareError {}

/// Read one report artifact from disk.
///
/// # Errors
///
/// Returns [`CompareError::Io`] when the file cannot be read and [`CompareError::Parse`] when it is
/// not a benchmark report.
pub fn read_report(path: &Path) -> Result<BenchReport, CompareError> {
	let text = std::fs::read_to_string(path).map_err(|e| CompareError::Io { path: path.to_path_buf(), reason: e.to_string() })?;
	serde_json::from_str(&text).map_err(|e| CompareError::Parse { path: path.to_path_buf(), reason: e.to_string() })
}

/// Compare a candidate report against a baseline report.
///
/// `threshold_pct` is a percentage; a non-finite or negative threshold is rejected so a typo in a
/// nightly input cannot silently turn the gate off.
///
/// # Errors
///
/// Returns [`CompareError::Incomparable`] when the two runs ran on different operating systems or
/// architectures and `ignore_provenance` is not set.
#[must_use]
pub fn compare_reports(baseline: &BenchReport, candidate: &BenchReport, threshold_pct: f64, ignore_provenance: bool) -> Result<Comparison, CompareError> {
	let notes = provenance(baseline, candidate);
	let hard: Vec<String> = notes.iter().filter(|note| note.starts_with("hard:")).map(|note| note.trim_start_matches("hard: ").to_string()).collect();
	if !ignore_provenance && !hard.is_empty() {
		return Err(CompareError::Incomparable(hard));
	}
	// A comparison forced through `ignore_provenance` still names what it ignored. A silent
	// escape hatch is how a cross-platform table ends up published with no caveat attached.
	let notes: Vec<String> = if ignore_provenance { notes.iter().map(|note| note.replace("hard: ", "ignored: ")).collect() } else { notes.into_iter().filter(|note| !note.starts_with("hard:")).collect() };
	let mut baseline_map: BTreeMap<WorkloadKey, &BenchResult> = BTreeMap::new();
	for result in &baseline.results {
		baseline_map.insert(key_of(result), result);
	}
	let mut candidate_map: BTreeMap<WorkloadKey, &BenchResult> = BTreeMap::new();
	for result in &candidate.results {
		candidate_map.insert(key_of(result), result);
	}
	let workloads = baseline_map.iter().filter_map(|(key, before)| candidate_map.get(key).map(|after| compare_workload(key, before, after, threshold_pct))).collect::<Vec<_>>();
	let only_in_baseline = baseline_map.keys().filter(|k| !candidate_map.contains_key(*k)).cloned().collect();
	let only_in_candidate = candidate_map.keys().filter(|k| !baseline_map.contains_key(*k)).cloned().collect();
	Ok(Comparison { threshold_pct, notes, workloads, only_in_baseline, only_in_candidate })
}

fn key_of(result: &BenchResult) -> WorkloadKey {
	WorkloadKey { profile: result.profile.clone(), adapter: result.adapter.clone(), workload: result.workload.clone(), shape: shape_of(&result.dataset) }
}

fn shape_of(dataset: &DatasetMeta) -> String {
	let shape = match &dataset.signal_shape {
		Some(signal) => serde_json::to_string(signal).unwrap_or_else(|_| "unknown".to_string()),
		None => "none".to_string(),
	};
	format!("in={} out={} irregular={} missing={} seed={} shape={}", dataset.input_points, dataset.output_points, dataset.irregular, format_pct(dataset.missingness_fraction * 100.0), dataset.seed, shape)
}

fn compare_workload(key: &WorkloadKey, before: &BenchResult, after: &BenchResult, threshold_pct: f64) -> WorkloadVerdict {
	// The measures, their names and their better-directions come from the manifest's grading, so
	// `weft-bench compare` and a run manifest's `grade` block cannot disagree about which of two
	// numbers wins. A gate watches the headline measures plus the correctness flag: a workload that
	// stopped being correct is a regression whatever the magnitudes say.
	let after_measures = crate::manifest::result_measures(after);
	let after_measures: BTreeMap<&str, &Measure> = after_measures.iter().map(|measure| (measure.name.as_str(), measure)).collect();
	let mut metrics = Vec::new();
	for before_measure in crate::manifest::result_measures(before) {
		if !(before_measure.headline || before_measure.name == "correctness.passed") {
			continue;
		}
		let Some(after_measure) = after_measures.get(before_measure.name.as_str()) else {
			continue;
		};
		// One schema version grades a given name in one unit; a different unit here would mean the
		// two reports were not written by the same schema, which `compare_reports` already refused.
		debug_assert_eq!(after_measure.unit, before_measure.unit, "one measure name changed units");
		let higher_is_better = matches!(before_measure.better, Better::Higher | Better::Pass);
		let gate = noise_gate_for(&before_measure.name, before, after, before_measure.better);
		metrics.push(move_of(before_measure.name, before_measure.value, after_measure.value, higher_is_better, gate, threshold_pct));
	}
	metrics.sort_by_key(|m| std::cmp::Reverse(m.verdict.rank()));
	let verdict = metrics.iter().map(|m| m.verdict).max_by_key(|v| v.rank()).unwrap_or(Verdict::Flat);
	WorkloadVerdict { key: key.clone(), metrics, verdict }
}

/// How much of this measure's move noise could explain.
///
/// A flag has no noise to consider. A latency percentile whose two runs both recorded a bootstrap
/// interval is gated on whether those intervals separate. Everything else — throughput, bytes per
/// point, an error figure — has no interval in the report schema, so nothing about its move can be
/// separated from noise and its honest ceiling is `Suspicious`.
fn noise_gate_for(name: &str, before: &BenchResult, after: &BenchResult, better: Better) -> NoiseGate {
	if better == Better::Pass {
		return NoiseGate::Certain;
	}
	let (Some(before_ci), Some(after_ci)) = (interval_of(before, name), interval_of(after, name)) else {
		return NoiseGate::NotMeasured;
	};
	if before_ci.upper_ns < after_ci.lower_ns || after_ci.upper_ns < before_ci.lower_ns {
		NoiseGate::Separated
	} else {
		NoiseGate::Overlap
	}
}

/// The bootstrap interval a result recorded for one measure name, when it recorded one.
fn interval_of(result: &BenchResult, name: &str) -> Option<ConfidenceInterval> {
	let intervals = result.latency_ci.as_ref()?;
	Some(match name {
		"latency.p50_ns" => intervals.p50.clone(),
		"latency.p95_ns" => intervals.p95.clone(),
		"latency.p99_ns" => intervals.p99.clone(),
		_ => return None,
	})
}

fn move_of(metric: String, before: f64, after: f64, higher_is_better: bool, noise: NoiseGate, threshold_pct: f64) -> MetricMove {
	let change_pct = if before == 0.0 { 0.0 } else { (after - before) / before * 100.0 };
	let worse = if higher_is_better { change_pct < 0.0 } else { change_pct > 0.0 };
	let better = if higher_is_better { change_pct > 0.0 } else { change_pct < 0.0 };
	let magnitude = change_pct.abs();
	let verdict = if magnitude <= threshold_pct {
		Verdict::Flat
	} else if worse {
		if noise.allows_regression() {
			Verdict::Regression
		} else {
			Verdict::Suspicious
		}
	} else if better {
		Verdict::Improved
	} else {
		Verdict::Flat
	};
	MetricMove { metric, higher_is_better, baseline: before, candidate: after, change_pct, worse_by_pct: if worse { magnitude } else { 0.0 }, noise, verdict }
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		adapter::SystemAdapter, report::RunMetadata, schema::{BenchResult, CorrectnessReport, DatasetMeta, TimingBreakdown}, stats::{ConfidenceInterval, LatencyCis, LatencyStats}
	};

	fn meta(os: &str, arch: &str) -> RunMetadata {
		RunMetadata { weft_bench_version: "0.1.0".into(), os: os.into(), arch: arch.into(), cpu_model: Some("EPYC 7501".into()), cpu_cores_physical: Some(16), cpu_cores_logical: Some(32), total_memory_bytes: Some(64 << 30), work_disk_kind: Some("nvme".into()), work_disk_file_system: Some("ext4".into()), work_disk_mount_point: Some("/mnt/deepmem".into()), generated_at: "2026-10-25T00:00:00+00:00".into(), engine: None }
	}

	fn dataset() -> DatasetMeta {
		DatasetMeta { input_points: 8192, output_points: 8192, irregular: true, missingness_fraction: 0.1, seed: 7, signal_shape: None }
	}

	fn latency(p50: u64, p95: u64, p99: u64) -> LatencyStats {
		LatencyStats { count: 30, min_ns: p50 / 2, max_ns: p99 * 2, mean_ns: p50, stddev_ns: p50 / 10, p50_ns: p50, p95_ns: p95, p99_ns: p99 }
	}

	/// A one-percent band around each centre, so tests can say exactly whether two runs'
	/// intervals overlap.
	fn ci(p50: u64, p95: u64, p99: u64) -> LatencyCis {
		LatencyCis { resamples: 1000, confidence: 0.99, seed: 0xB007_5EED, mean: interval(p50), p50: interval(p50), p95: interval(p95), p99: interval(p99) }
	}

	fn interval(centre: u64) -> ConfidenceInterval {
		ConfidenceInterval { point_ns: centre, lower_ns: centre - centre / 100, upper_ns: centre + centre / 100 }
	}

	/// The same shape of interval widened to `percent` either side, for a run noisy enough to
	/// swallow a move that looks real.
	fn wide_ci(centres: [u64; 3], percent: u64) -> LatencyCis {
		let band = |centre: u64| ConfidenceInterval { point_ns: centre, lower_ns: centre - centre * percent / 100, upper_ns: centre + centre * percent / 100 };
		LatencyCis { resamples: 1000, confidence: 0.99, seed: 0xB007_5EED, mean: band(centres[0]), p50: band(centres[0]), p95: band(centres[1]), p99: band(centres[2]) }
	}

	fn result(workload: &str, p50: u64, p95: u64, p99: u64, throughput: f64, intervals: Option<LatencyCis>) -> BenchResult {
		BenchResult { schema_version: crate::schema::SCHEMA_VERSION, profile: "standard".into(), adapter: "weft".into(), workload: workload.into(), reps: 3, dataset: dataset(), latency: latency(p50, p95, p99), latency_ci: intervals, throughput_points_per_sec: throughput, timing: TimingBreakdown { dataset_generation_ns: 0, measured_ns: 0, end_to_end_ns: 0 }, correctness: CorrectnessReport { output_count_ok: true, expected_output_points: 1, actual_output_points: 1, values_finite: true }, accuracy: None, storage: None, cold_warm: None, gpu_pool: None, adapter_info: None }
	}

	fn report(os: &str, arch: &str, results: Vec<BenchResult>) -> BenchReport {
		BenchReport { schema_version: crate::schema::SCHEMA_VERSION, metadata: meta(os, arch), results }
	}

	#[test]
	fn a_beyond_threshold_move_with_overlapping_intervals_is_only_suspicious() {
		// p50 is 20% worse, but both runs carry a +-25% band, so the bands still overlap.
		let baseline = report("linux", "x86_64", vec![result("query", 1_000, 2_000, 3_000, 1e6, Some(wide_ci([1_000, 2_000, 3_000], 25)))]);
		let candidate = report("linux", "x86_64", vec![result("query", 1_200, 2_020, 3_030, 1e6, Some(wide_ci([1_200, 2_020, 3_030], 25)))]);
		let comparison = compare_reports(&baseline, &candidate, DEFAULT_THRESHOLD_PCT, false).expect("comparable");
		assert_eq!(comparison.workloads.len(), 1);
		assert_eq!(comparison.workloads[0].verdict, Verdict::Suspicious);
		assert!(!comparison.failed(false), "a move inside the noise must not fail a nightly");
		assert!(comparison.failed(true), "--strict reports it anyway");
		let p95 = comparison.workloads[0].metrics.iter().find(|m| m.metric == "latency.p95_ns").expect("p95 compared");
		assert_eq!(p95.noise, NoiseGate::Overlap);
		assert!((p95.change_pct - 1.0).abs() < 1e-9, "2000 -> 2020 is +1%, got {p95:?}");
	}

	#[test]
	fn a_beyond_threshold_move_with_disjoint_intervals_is_a_regression() {
		let baseline = report("linux", "x86_64", vec![result("query", 1_000, 2_000, 3_000, 1e6, Some(ci(1_000, 2_000, 3_000)))]);
		// Every latency is 40% worse and its interval clears the baseline's upper bound.
		let candidate = report("linux", "x86_64", vec![result("query", 1_400, 2_800, 4_200, 1e6, Some(ci(1_400, 2_800, 4_200)))]);
		let comparison = compare_reports(&baseline, &candidate, DEFAULT_THRESHOLD_PCT, false).expect("comparable");
		assert_eq!(comparison.workloads[0].verdict, Verdict::Regression);
		assert!(comparison.failed(false));
		let p95 = comparison.workloads[0].metrics.iter().find(|m| m.metric == "latency.p95_ns").expect("p95 compared");
		assert_eq!(p95.noise, NoiseGate::Separated);
		assert!((p95.worse_by_pct - 40.0).abs() < 1e-9);
	}

	#[test]
	fn a_move_within_the_threshold_is_flat() {
		let baseline = report("linux", "x86_64", vec![result("query", 1_000, 2_000, 3_000, 1e6, Some(ci(1_000, 2_000, 3_000)))]);
		let candidate = report("linux", "x86_64", vec![result("query", 1_050, 2_090, 3_150, 1e6, Some(ci(1_050, 2_090, 3_150)))]);
		let comparison = compare_reports(&baseline, &candidate, DEFAULT_THRESHOLD_PCT, false).expect("comparable");
		assert_eq!(comparison.workloads[0].verdict, Verdict::Flat);
		assert!(!comparison.failed(true));
	}

	#[test]
	fn an_improvement_beyond_the_threshold_is_not_a_failure() {
		let baseline = report("linux", "x86_64", vec![result("query", 1_000, 2_000, 3_000, 1e6, Some(ci(1_000, 2_000, 3_000)))]);
		let candidate = report("linux", "x86_64", vec![result("query", 600, 1_200, 1_800, 1e6, Some(ci(600, 1_200, 1_800)))]);
		let comparison = compare_reports(&baseline, &candidate, DEFAULT_THRESHOLD_PCT, false).expect("comparable");
		assert_eq!(comparison.workloads[0].verdict, Verdict::Improved);
		assert!(!comparison.failed(true));
		assert_eq!(comparison.improvements().len(), 1);
	}

	#[test]
	fn throughput_without_an_interval_cannot_be_a_regression() {
		// 20% slower to ingest, and no bootstrap interval exists for throughput at all.
		let baseline = report("linux", "x86_64", vec![result("ingest", 1_000, 2_000, 3_000, 1e6, Some(ci(1_000, 2_000, 3_000)))]);
		let candidate = report("linux", "x86_64", vec![result("ingest", 1_000, 2_000, 3_000, 8e5, Some(ci(1_000, 2_000, 3_000)))]);
		let comparison = compare_reports(&baseline, &candidate, DEFAULT_THRESHOLD_PCT, false).expect("comparable");
		let moved = comparison.workloads[0].metrics.iter().find(|m| m.metric == "throughput.points_per_sec").expect("throughput compared");
		assert_eq!(moved.verdict, Verdict::Suspicious);
		assert_eq!(moved.noise, NoiseGate::NotMeasured);
		assert!((moved.worse_by_pct - 20.0).abs() < 1e-9);
		assert!(!comparison.failed(false));
	}

	#[test]
	fn a_run_without_recorded_intervals_stops_at_suspicious() {
		let baseline = report("linux", "x86_64", vec![result("query", 1_000, 2_000, 3_000, 1e6, None)]);
		let candidate = report("linux", "x86_64", vec![result("query", 1_400, 2_800, 4_200, 1e6, None)]);
		let comparison = compare_reports(&baseline, &candidate, DEFAULT_THRESHOLD_PCT, false).expect("comparable");
		assert_eq!(comparison.workloads[0].verdict, Verdict::Suspicious);
		assert!(!comparison.failed(false));
	}

	#[test]
	fn different_platforms_are_refused_rather_than_caveated() {
		let baseline = report("linux", "x86_64", vec![result("query", 1_000, 2_000, 3_000, 1e6, Some(ci(1_000, 2_000, 3_000)))]);
		let candidate = report("macos", "aarch64", vec![result("query", 700, 1_400, 2_100, 1e6, Some(ci(700, 1_400, 2_100)))]);
		let err = compare_reports(&baseline, &candidate, DEFAULT_THRESHOLD_PCT, false).expect_err("must refuse");
		assert!(matches!(err, CompareError::Incomparable(_)), "{err}");
		let text = err.to_string();
		assert!(text.contains("os linux vs macos") && text.contains("architecture x86_64 vs aarch64"), "{text}");
		// The escape hatch is explicit, and says so in its notes.
		let forced = compare_reports(&baseline, &candidate, DEFAULT_THRESHOLD_PCT, true).expect("--ignore-provenance compares");
		assert!(forced.notes.iter().any(|n| n.contains("os linux vs macos")), "{:?}", forced.notes);
		assert!(forced.notes.iter().all(|n| !n.starts_with("hard:")));
	}

	#[test]
	fn a_comparison_that_paired_nothing_fails_the_gate() {
		// The seed is part of the pairing key, so two runs generated with different seeds share
		// no workload. That has to read as a failure, not as an empty table and a green check.
		let baseline = report("linux", "x86_64", vec![result("query", 1_000, 2_000, 3_000, 1e6, Some(ci(1_000, 2_000, 3_000)))]);
		let mut other = result("query", 1_000, 2_000, 3_000, 1e6, Some(ci(1_000, 2_000, 3_000)));
		other.dataset.seed = 11;
		let candidate = report("linux", "x86_64", vec![other]);
		let comparison = compare_reports(&baseline, &candidate, DEFAULT_THRESHOLD_PCT, false).expect("same platform");
		assert!(comparison.workloads.is_empty(), "the seed changed, so nothing pairs");
		assert!(comparison.failed(false), "comparing nothing is not a pass");
		assert!(comparison.failed(true));
		assert!(!comparison.only_in_baseline.is_empty() && !comparison.only_in_candidate.is_empty(), "and it says which side each workload came from");
	}

	#[test]
	fn a_different_cpu_is_a_note_not_a_refusal() {
		let mut baseline = report("linux", "x86_64", vec![result("query", 1_000, 2_000, 3_000, 1e6, Some(ci(1_000, 2_000, 3_000)))]);
		baseline.metadata.cpu_model = Some("EPYC 7501".into());
		let mut candidate = report("linux", "x86_64", vec![result("query", 1_000, 2_000, 3_000, 1e6, Some(ci(1_000, 2_000, 3_000)))]);
		candidate.metadata.cpu_model = Some("EPYC 9354".into());
		let comparison = compare_reports(&baseline, &candidate, DEFAULT_THRESHOLD_PCT, false).expect("a hosted runner changes cpu nightly");
		assert_eq!(comparison.workloads[0].verdict, Verdict::Flat);
		assert!(comparison.notes.iter().any(|n| n.contains("cpu: EPYC 7501 vs EPYC 9354")), "{:?}", comparison.notes);
	}

	#[test]
	fn a_different_adapter_is_a_note_that_names_both_engines() {
		// The nightly gate runs the same adapters on both sides, so it says nothing
		// about them: the note only appears when the engines actually differ.
		let baseline = report("linux", "x86_64", vec![reported("query", 1_000, 2_000, 3_000)]);
		let unchanged = compare_reports(&baseline, &baseline, DEFAULT_THRESHOLD_PCT, false).expect("comparable");
		assert!(unchanged.notes.iter().all(|n| !n.starts_with("adapter:")), "{:?}", unchanged.notes);

		// Pairing WeftDB with another engine is the point of a competitor
		// comparison, so it is allowed -- but the note has to name both engines and
		// what each does with the requested spline, or the table reads as a
		// like-for-like race when it is not one.
		let mut competitor = report("linux", "x86_64", vec![result("query", 1_000, 2_000, 3_000, 1e6, None)]);
		competitor.results[0].adapter = "duckdb".into();
		competitor.results[0].adapter_info = Some(crate::adapter::AdapterInfo { interface_version: 2, engine: "duckdb".into(), method: "native interpolation".into(), native_engine: true, honours_requested_spline: false });
		let comparison = compare_reports(&baseline, &competitor, DEFAULT_THRESHOLD_PCT, false).expect("a cross-engine comparison is allowed");
		let note = comparison.notes.iter().find(|n| n.starts_with("adapter:")).expect("the adapters differ").clone();
		assert!(note.contains("weft [engine splimes") && note.contains("honours the requested spline"), "weftdb side of: {note}");
		assert!(note.contains("duckdb [engine duckdb") && note.contains("ignores the requested spline"), "competitor side of: {note}");

		// An artifact written before adapters reported themselves is distinguishable
		// from one that carries a report, and says so.
		let legacy = report("linux", "x86_64", vec![result("query", 1_000, 2_000, 3_000, 1e6, None)]);
		let comparison = compare_reports(&baseline, &legacy, DEFAULT_THRESHOLD_PCT, false).expect("comparable");
		let note = comparison.notes.iter().find(|n| n.starts_with("adapter:")).expect("one side reports, the other cannot").clone();
		assert!(note.contains("[no adapter report]"), "{note}");
	}

	/// The same result as [`result`], with the report WeftDB's own adapter gives.
	fn reported(workload: &str, p50: u64, p95: u64, p99: u64) -> BenchResult {
		let mut result = result(workload, p50, p95, p99, 1e6, None);
		result.adapter_info = Some(crate::WeftAdapter::new().adapter_info());
		result
	}

	#[test]
	fn only_results_present_in_both_runs_are_compared() {
		let baseline = report("linux", "x86_64", vec![result("query", 1_000, 2_000, 3_000, 1e6, None), result("ingest", 1_000, 2_000, 3_000, 1e6, None)]);
		let candidate = report("linux", "x86_64", vec![result("query", 1_000, 2_000, 3_000, 1e6, None), result("compaction", 1_000, 2_000, 3_000, 1e6, None)]);
		let comparison = compare_reports(&baseline, &candidate, DEFAULT_THRESHOLD_PCT, false).expect("comparable");
		assert_eq!(comparison.workloads.len(), 1);
		assert_eq!(comparison.workloads[0].key.workload, "query");
		assert_eq!(comparison.only_in_baseline.len(), 1);
		assert_eq!(comparison.only_in_candidate.len(), 1);
		assert!(comparison.tally().contains("2 unpaired"), "{}", comparison.tally());
	}

	#[test]
	fn a_different_dataset_shape_is_a_different_workload() {
		let mut bigger = result("query", 1_000, 2_000, 3_000, 1e6, None);
		bigger.dataset.input_points = 16_384;
		let baseline = report("linux", "x86_64", vec![result("query", 1_000, 2_000, 3_000, 1e6, None)]);
		let candidate = report("linux", "x86_64", vec![bigger]);
		let comparison = compare_reports(&baseline, &candidate, DEFAULT_THRESHOLD_PCT, false).expect("comparable");
		assert!(comparison.workloads.is_empty(), "a changed shape is not the same measurement");
		assert_eq!(comparison.only_in_baseline.len(), 1);
		assert_eq!(comparison.only_in_candidate.len(), 1);
	}

	#[test]
	fn a_baseline_of_zero_reports_no_move_rather_than_infinity() {
		let baseline = report("linux", "x86_64", vec![result("query", 0, 0, 0, 0.0, None)]);
		let candidate = report("linux", "x86_64", vec![result("query", 500, 900, 1_400, 1e6, None)]);
		let comparison = compare_reports(&baseline, &candidate, DEFAULT_THRESHOLD_PCT, false).expect("comparable");
		assert_eq!(comparison.workloads[0].verdict, Verdict::Flat);
		for metric in &comparison.workloads[0].metrics {
			assert!(metric.change_pct.is_finite(), "{metric:?}");
		}
	}

	#[test]
	fn the_worst_metric_decides_the_workload_verdict() {
		let baseline = report("linux", "x86_64", vec![result("query", 1_000, 2_000, 3_000, 1e6, Some(ci(1_000, 2_000, 3_000)))]);
		// p50 improves and p95 regresses hard with disjoint intervals.
		let candidate = report("linux", "x86_64", vec![result("query", 800, 3_000, 4_500, 1e6, Some(ci(800, 3_000, 4_500)))]);
		let comparison = compare_reports(&baseline, &candidate, DEFAULT_THRESHOLD_PCT, false).expect("comparable");
		assert_eq!(comparison.workloads[0].verdict, Verdict::Regression);
		assert_eq!(comparison.workloads[0].metrics.first().expect("sorted worst first").metric, "latency.p95_ns");
	}

	#[test]
	fn the_measures_compared_are_the_ones_the_manifest_would_grade() {
		// `weft-bench verify` grades a run from the manifest's `grade` block; `compare` watches the
		// headline members of that same block, so neither command can be quietly watching a column
		// the other one ignores. p99 is graded but not headline, so it must NOT appear here.
		let baseline = report("linux", "x86_64", vec![result("query", 1_000, 2_000, 3_000, 1e6, Some(ci(1_000, 2_000, 3_000)))]);
		let candidate = report("linux", "x86_64", vec![result("query", 1_000, 2_000, 9_000, 1e6, Some(ci(1_000, 2_000, 3_000)))]);
		let comparison = compare_reports(&baseline, &candidate, DEFAULT_THRESHOLD_PCT, false).expect("comparable");
		let names: Vec<&str> = comparison.workloads[0].metrics.iter().map(|m| m.metric.as_str()).collect();
		// The order is the manifest's own grading order: the sort is worst-metric-first and is stable,
		// so a run with nothing to report keeps the order the measures were graded in.
		assert_eq!(names, ["latency.p50_ns", "latency.mean_ns", "latency.p95_ns", "throughput.points_per_sec", "correctness.passed"]);
	}

	#[test]
	fn a_run_that_stopped_being_correct_is_a_regression_however_small() {
		// A flag has no magnitude and no bootstrap interval, so the noise gate does not get a vote:
		// a workload that was correct and is not any more fails the gate at any threshold.
		let baseline = report("linux", "x86_64", vec![result("query", 1_000, 2_000, 3_000, 1e6, Some(ci(1_000, 2_000, 3_000)))]);
		let mut broken = result("query", 1_000, 2_000, 3_000, 1e6, Some(ci(1_000, 2_000, 3_000)));
		broken.correctness.values_finite = false;
		let candidate = report("linux", "x86_64", vec![broken]);
		let comparison = compare_reports(&baseline, &candidate, DEFAULT_THRESHOLD_PCT, false).expect("comparable");
		let flag = comparison.workloads[0].metrics.iter().find(|m| m.metric == "correctness.passed").expect("correctness compared");
		assert_eq!(flag.noise, NoiseGate::Certain);
		assert_eq!(flag.verdict, Verdict::Regression);
		assert_eq!(comparison.workloads[0].verdict, Verdict::Regression);
		assert!(comparison.failed(false));
	}

	#[test]
	fn reports_of_different_schema_versions_are_refused() {
		// Column meanings move between schema versions; a verdict computed across one would be
		// comparing two different definitions of the same name.
		let mut candidate = report("linux", "x86_64", vec![result("query", 1_000, 2_000, 3_000, 1e6, Some(ci(1_000, 2_000, 3_000)))]);
		candidate.schema_version += 1;
		let baseline = report("linux", "x86_64", vec![result("query", 1_000, 2_000, 3_000, 1e6, Some(ci(1_000, 2_000, 3_000)))]);
		let refused = compare_reports(&baseline, &candidate, DEFAULT_THRESHOLD_PCT, false).expect_err("different schema versions");
		assert!(matches!(&refused, CompareError::Incomparable(notes) if notes.iter().any(|n| n.contains("schema"))), "{refused:?}");
		let forced = compare_reports(&baseline, &candidate, DEFAULT_THRESHOLD_PCT, true).expect("--ignore-provenance overrides");
		assert!(forced.notes.iter().any(|n| n.starts_with("ignored: report schema")), "{:?}", forced.notes);
	}

	#[test]
	fn markdown_and_json_describe_the_same_verdict() {
		let baseline = report("linux", "x86_64", vec![result("query", 1_000, 2_000, 3_000, 1e6, Some(ci(1_000, 2_000, 3_000)))]);
		let candidate = report("linux", "x86_64", vec![result("query", 1_400, 2_800, 4_200, 1e6, Some(ci(1_400, 2_800, 4_200)))]);
		let comparison = compare_reports(&baseline, &candidate, DEFAULT_THRESHOLD_PCT, false).expect("comparable");
		let markdown = comparison.to_markdown();
		assert!(markdown.contains("| **regression** |"), "{markdown}");
		assert!(markdown.contains("Threshold 10%"), "{markdown}");
		let json: serde_json::Value = serde_json::from_str(&comparison.to_json().expect("serialisable")).expect("round trips");
		assert_eq!(json["workloads"][0]["verdict"], "regression");
		assert_eq!(json["workloads"][0]["metrics"][0]["noise"], "separated");
	}

	#[test]
	fn reading_a_report_reports_the_path_that_failed() {
		let missing = std::path::Path::new("/nonexistent/weft-bench-report.json");
		let err = read_report(missing).expect_err("missing file");
		assert!(matches!(err, CompareError::Io { .. }), "{err}");
		assert!(err.to_string().contains("/nonexistent/weft-bench-report.json"), "{err}");
		let dir = std::env::temp_dir().join(format!("weft-compare-parse-{}", std::process::id()));
		std::fs::create_dir_all(&dir).expect("temp dir");
		let junk = dir.join("report.json");
		std::fs::write(&junk, b"{ not json").expect("write");
		let err = read_report(&junk).expect_err("junk file");
		assert!(matches!(err, CompareError::Parse { .. }), "{err}");
		let ok = dir.join("good.json");
		std::fs::write(&ok, serde_json::to_vec(&report("linux", "x86_64", vec![result("query", 1, 2, 3, 1.0, None)])).expect("serialise")).expect("write");
		assert_eq!(read_report(&ok).expect("reads back").results.len(), 1);
		let _ = std::fs::remove_dir_all(&dir);
	}
}

/// Differences between the two runs' environment blocks. Entries prefixed `hard:` are the ones
/// that make a comparison meaningless rather than merely worth a caveat.
fn provenance(baseline: &BenchReport, candidate: &BenchReport) -> Vec<String> {
	let mut notes = Vec::new();
	let b = &baseline.metadata;
	let c = &candidate.metadata;
	if b.os != c.os {
		notes.push(format!("hard: os {} vs {}", b.os, c.os));
	}
	if b.arch != c.arch {
		notes.push(format!("hard: architecture {} vs {}", b.arch, c.arch));
	}
	note_diff(&mut notes, "cpu", &b.cpu_model, &c.cpu_model);
	note_diff(&mut notes, "physical cores", &b.cpu_cores_physical.map(|v| v.to_string()), &c.cpu_cores_physical.map(|v| v.to_string()));
	note_diff(&mut notes, "total memory", &b.total_memory_bytes.map(|v| (v / (1 << 30)).to_string()).map(|v| format!("{v} GiB")), &c.total_memory_bytes.map(|v| format!("{} GiB", v / (1 << 30))));
	note_diff(&mut notes, "work disk", &b.work_disk_kind, &c.work_disk_kind);
	note_diff(&mut notes, "work filesystem", &b.work_disk_file_system, &c.work_disk_file_system);
	if b.weft_bench_version != c.weft_bench_version {
		notes.push(format!("weft-bench {} vs {}", b.weft_bench_version, c.weft_bench_version));
	}
	if baseline.schema_version != candidate.schema_version {
		// Two reports of different schema versions do not necessarily measure the same thing in the
		// same column, so this is a refusal and not a caveat -- `--ignore-provenance` is the override.
		notes.push(format!("hard: report schema v{} vs v{}", baseline.schema_version, candidate.schema_version));
	}
	// Which adapters produced the numbers is provenance too, and now that every
	// adapter reports itself it is recorded rather than assumed. It is a caveat and
	// never a refusal: pairing WeftDB with another engine is the point of a
	// competitor comparison. What must not happen is a table that reads like a
	// like-for-like race while one side ran a native engine and the other an
	// in-process baseline, or like a machine difference that is really an
	// interface difference.
	let baseline_adapters = adapters_of(baseline);
	let candidate_adapters = adapters_of(candidate);
	if baseline_adapters != candidate_adapters {
		notes.push(format!("adapter: {} vs {}", baseline_adapters.join(" + "), candidate_adapters.join(" + ")));
	}
	notes
}

/// The distinct engines behind a report's results, each spelled out with what it
/// reported, as one sorted set so two reports compare by content. A result with
/// no report is named as such: that shape only exists in artifacts written before
/// the reporting interface, and in the workloads that do not drive an adapter.
fn adapters_of(report: &BenchReport) -> Vec<String> {
	let mut adapters: Vec<String> = report
		.results
		.iter()
		.map(|result| match &result.adapter_info {
			Some(info) => format!("{} [engine {}; {}; {}; {}]", result.adapter, info.engine, info.method, engine_kind(info.native_engine), spline_honour(info.honours_requested_spline)),
			None => format!("{} [no adapter report]", result.adapter),
		})
		.collect();
	adapters.sort_unstable();
	adapters.dedup();
	adapters
}

fn engine_kind(native_engine: bool) -> &'static str {
	if native_engine {
		"native engine"
	} else {
		"in-process baseline"
	}
}

fn spline_honour(honours_requested_spline: bool) -> &'static str {
	if honours_requested_spline {
		"honours the requested spline"
	} else {
		"ignores the requested spline"
	}
}

fn note_diff(notes: &mut Vec<String>, label: &str, before: &Option<String>, after: &Option<String>) {
	if before != after {
		notes.push(format!("{label}: {} vs {}", before.as_deref().unwrap_or("unrecorded"), after.as_deref().unwrap_or("unrecorded")));
	}
}
