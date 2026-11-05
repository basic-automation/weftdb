//! Pool separate runs of the cross-engine benchmark into one report (#147).
//!
//! The first cross-engine comparison ran twice on one host and found 53 of 144 rows whose p50
//! intervals did not overlap between the runs (DECISIONS.md, EVI-15): each run's bootstrap bounds
//! where *that run's* median sits, not where the next run's will. `weft-xbench combine` pools each
//! result's raw samples across runs and redraws its intervals runs-first
//! ([`LatencyStats::clustered_bootstrap_cis`]), so the run-to-run spread is inside them, and records
//! how the runs differed in [`ArmResult::between_run`]. The renderer then calls a win or a loss
//! only where the pooled intervals separate *and* every run's median falls the same way.
//!
//! The runs must be the same benchmark: the same results in the same order, from the same seed and
//! input files, on the same machine, each with its raw samples (report v5). A run whose answer
//! differs from the others' (`answer_sha256`) fails the combined gate.

use anyhow::{bail, ensure, Context as _};
use weft_bench::{
	schema::BetweenRun, stats::{BootstrapConfig, LatencyStats}
};

use crate::{ArmResult, InputFile, XbenchReport, XBENCH_SCHEMA_VERSION};

/// The seed the combined intervals are drawn with: the runs' own, as [`crate::latency`] mixes it.
const fn bootstrap_seed(seed: u64) -> u64 {
	seed ^ 0x_C0FF_EE15_C0DE_u64
}

/// Pool `runs`, read from `sources`, into one report.
///
/// Each combined result is the run with the median p50, with every run's samples pooled in run
/// order, its latency recomputed from them, its intervals drawn runs-first and its reps summed.
/// The metadata and manifest are the first run's; `combined_from` names every run.
///
/// # Errors
///
/// Fewer than two runs, runs that are not the same benchmark, or a result without raw samples.
pub fn combine(runs: &[XbenchReport], sources: Vec<InputFile>) -> anyhow::Result<XbenchReport> {
	ensure!(runs.len() >= 2, "combining needs at least two runs, got {}", runs.len());
	let first = &runs[0];
	for (n, run) in runs.iter().enumerate().skip(1) {
		let (a, b) = (&first.metadata, &run.metadata);
		ensure!(a.os == b.os && a.arch == b.arch && a.cpu_model == b.cpu_model, "run {} ran on another machine than run 1 ({:?} vs {:?}): a pooled interval would measure the machines", n + 1, b.cpu_model, a.cpu_model);
		ensure!(run.seed == first.seed, "run {} used seed {}, run 1 seed {}", n + 1, run.seed, first.seed);
		let hashes = |r: &XbenchReport| r.inputs.iter().map(|i| (i.role.clone(), i.sha256.clone())).collect::<Vec<_>>();
		ensure!(hashes(run) == hashes(first), "run {} read different input files from run 1", n + 1);
		ensure!(run.results.len() == first.results.len(), "run {} holds {} results, run 1 holds {}", n + 1, run.results.len(), first.results.len());
		for (x, y) in first.results.iter().zip(&run.results) {
			ensure!(key(x) == key(y), "run {} has {} where run 1 has {}", n + 1, label(y), label(x));
		}
	}
	let results = (0..first.results.len()).map(|i| combine_result(&runs.iter().map(|run| &run.results[i]).collect::<Vec<_>>(), first.seed)).collect::<anyhow::Result<Vec<_>>>()?;
	Ok(XbenchReport { schema_version: XBENCH_SCHEMA_VERSION, results, combined_from: sources, ..first.clone() })
}

const fn key(r: &ArmResult) -> (&str, &str, crate::Phase, crate::Warmth, usize) {
	(r.dataset.as_str(), r.arm.arm.as_str(), r.phase, r.warmth, r.input_points)
}

fn label(r: &ArmResult) -> String {
	format!("{} / {} / {:?} / {}", r.dataset, r.arm.arm, r.phase, r.warmth.as_str())
}

fn combine_result(runs: &[&ArmResult], seed: u64) -> anyhow::Result<ArmResult> {
	for (n, run) in runs.iter().enumerate() {
		if run.samples_ns.is_empty() {
			bail!("{}: run {} carries no raw samples (a report before v5)", label(run), n + 1);
		}
	}
	let mut by_p50: Vec<usize> = (0..runs.len()).collect();
	by_p50.sort_by_key(|&i| runs[i].latency.p50_ns);
	let mut combined = runs[by_p50[runs.len() / 2]].clone();

	let samples: Vec<Vec<u64>> = runs.iter().map(|run| run.samples_ns.clone()).collect();
	combined.samples_ns = samples.concat();
	combined.latency = LatencyStats::from_samples(&combined.samples_ns);
	combined.latency_ci = LatencyStats::clustered_bootstrap_cis(&samples, &BootstrapConfig { seed: bootstrap_seed(seed), ..BootstrapConfig::default() });
	combined.reps = runs.iter().map(|run| run.reps).sum();

	let digests: Vec<Option<&str>> = runs.iter().map(|run| run.correctness.as_ref().and_then(|c| c.answer_sha256.as_deref())).collect();
	let digests_agree = digests.windows(2).all(|pair| pair[0] == pair[1]);
	if let Some(gate) = &mut combined.correctness {
		gate.passed = runs.iter().all(|run| run.correctness.as_ref().is_some_and(|c| c.passed)) && digests_agree;
		gate.max_abs_diff_vs_reference = runs.iter().filter_map(|run| run.correctness.as_ref()).map(|c| c.max_abs_diff_vs_reference).fold(0.0, f64::max);
		if !digests_agree {
			gate.answer_sha256 = None;
		}
	}
	let run_p50_ns: Vec<u64> = runs.iter().map(|run| run.latency.p50_ns).collect();
	let (fastest, slowest) = (run_p50_ns.iter().copied().min().unwrap_or(0), run_p50_ns.iter().copied().max().unwrap_or(0));
	let p50_spread = if fastest > 0 { crate::as_f64(slowest) / crate::as_f64(fastest) - 1.0 } else { 0.0 };
	combined.between_run = Some(BetweenRun { runs: runs.len(), run_p50_ns, run_mean_ns: runs.iter().map(|run| run.latency.mean_ns).collect(), p50_spread, digests_agree });
	Ok(combined)
}

/// Read the reports at `paths`, in order, each with its file hash for `combined_from`.
///
/// # Errors
///
/// A path that cannot be read or does not hold a report.
pub fn read_runs(paths: &[std::path::PathBuf]) -> anyhow::Result<(Vec<XbenchReport>, Vec<InputFile>)> {
	let mut runs = Vec::with_capacity(paths.len());
	let mut sources = Vec::with_capacity(paths.len());
	for path in paths {
		let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
		runs.push(serde_json::from_slice(&bytes).with_context(|| format!("{} is not a weft-xbench report", path.display()))?);
		sources.push(InputFile::hash("run", path)?);
	}
	Ok((runs, sources))
}

#[cfg(test)]
mod tests {
	use weft_bench::RunMetadata;

	use super::*;
	use crate::{latency, ArmInfo, Correctness, DatasetKind, Phase, Warmth};

	fn result(level: u64, digest: &str) -> ArmResult {
		let samples: Vec<u64> = (0..12).map(|i| level + i % 4).collect();
		let (latency, latency_ci) = latency(&samples, 7);
		let correctness = Some(Correctness { expected_points: 3, actual_points: 3, values_finite: true, max_abs_diff_vs_reference: 0.0, tolerance: 1e-9, passed: true, unfilled_points: 0, answer_sha256: Some(digest.into()) });
		ArmResult { dataset: "mcar-tiny".into(), kind: DatasetKind::Mcar, scale: "tiny".into(), input_points: 10, output_points: 3, missingness_fraction: 0.2, arm: ArmInfo { arm: "duckdb-double".into(), engine: "e".into(), value_type: "v".into(), method: "m".into(), class: None, own_answer: None }, phase: Phase::Interpolate, reps: samples.len(), latency, latency_ci, correctness, accuracy: None, storage_bytes: None, warmth: Warmth::Warm, samples_ns: samples, between_run: None }
	}

	fn run(level: u64, digest: &str) -> XbenchReport {
		XbenchReport { schema_version: XBENCH_SCHEMA_VERSION, metadata: RunMetadata::capture("2026-11-02T00:00:00Z".into()), engines: vec![], seed: 7, source_commit: None, invocation: vec![], scratch_dir: None, optimized: Some(true), cold_hook: None, results: vec![result(level, digest)], inputs: vec![], binary_sha256: None, containers: vec![], combined_from: vec![] }
	}

	#[test]
	fn runs_pool_and_the_interval_spans_them() {
		let combined = combine(&[run(1_000, "a"), run(1_300, "a"), run(1_100, "a")], vec![]).expect("combines");
		let r = &combined.results[0];
		assert_eq!((r.reps, r.samples_ns.len()), (36, 36));
		let between = r.between_run.as_ref().expect("between-run figures");
		assert_eq!(between.run_p50_ns, vec![1_001, 1_301, 1_101]);
		assert!(between.digests_agree && r.correctness.as_ref().is_some_and(|c| c.passed));
		assert!(r.latency_ci.p50.upper_ns - r.latency_ci.p50.lower_ns > 100, "{:?}", r.latency_ci.p50);
	}

	#[test]
	fn a_run_that_answered_differently_fails_the_gate() {
		let combined = combine(&[run(1_000, "a"), run(1_000, "b")], vec![]).expect("combines");
		let gate = combined.results[0].correctness.as_ref().expect("gate");
		assert!(!gate.passed && gate.answer_sha256.is_none());
		assert!(!combined.all_correct());
	}

	#[test]
	fn only_runs_of_the_same_benchmark_combine() {
		assert!(combine(&[run(1_000, "a")], vec![]).is_err());
		let mut other_seed = run(1_000, "a");
		other_seed.seed = 8;
		assert!(combine(&[run(1_000, "a"), other_seed], vec![]).is_err());
		let mut other_input = run(1_000, "a");
		other_input.inputs.push(InputFile { role: "uci".into(), path: "x".into(), bytes: 1, sha256: "00".into() });
		assert!(combine(&[run(1_000, "a"), other_input], vec![]).is_err());
		let mut no_samples = run(1_000, "a");
		no_samples.results[0].samples_ns.clear();
		assert!(combine(&[run(1_000, "a"), no_samples], vec![]).expect_err("needs samples").to_string().contains("v5"));
	}
}
