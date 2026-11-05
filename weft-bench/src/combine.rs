//! Pool separate runs of one benchmark into a single report (fair protocol, roadmap 1.1).
//!
//! A report's bootstrap intervals describe one run: every sample in it shares that run's machine
//! state, so they bound where *that run's* median sits, not where the next run's will. The first
//! cross-engine report found as much the hard way — two runs on the same host had disjoint p50
//! intervals on 53 of 144 rows (DECISIONS.md, EVI-15). `combine` takes the reports of several
//! separate invocations, pools each result's raw samples, and recomputes its intervals with a
//! two-level bootstrap ([`LatencyStats::clustered_bootstrap_cis`]) that draws runs before
//! samples, so the spread between runs is inside the interval. `weft-bench compare` calls a move
//! a regression only when two reports' intervals are disjoint, so comparing combined reports holds
//! a change to the run-to-run noise without any change of its own.
//!
//! The runs must be the same benchmark: the same results in the same order, over the same dataset,
//! on the same platform. Each must carry its raw samples (report schema v28). A run whose answer
//! differs from the others' (`result_sha256`) fails the combined result's correctness: the same
//! seeded benchmark answering differently is a defect, not noise.

use anyhow::{bail, ensure, Context as _};

use crate::{
	report::BenchReport, schema::{BenchResult, BetweenRun}, stats::{BootstrapConfig, LatencyStats}
};

/// Pool `reports`, separate runs of one benchmark, into one report.
///
/// Each combined result is the run with the median p50, its samples replaced by every run's
/// pooled in run order, its latency recomputed from them, its intervals drawn run-first when the
/// runs carried intervals at all, and its timings summed; `between_run` records how the runs
/// differed. The metadata is the first report's.
///
/// # Errors
///
/// Fewer than two reports; reports that are not runs of the same benchmark (different results,
/// datasets, operating systems, architectures or CPUs); or a result without raw samples.
pub fn combine(reports: &[BenchReport]) -> anyhow::Result<BenchReport> {
	ensure!(reports.len() >= 2, "combining needs at least two runs, got {}", reports.len());
	let first = &reports[0];
	ensure!(!first.results.is_empty(), "the first run holds no results");
	for (n, report) in reports.iter().enumerate().skip(1) {
		let (a, b) = (&first.metadata, &report.metadata);
		ensure!(a.os == b.os && a.arch == b.arch && a.cpu_model == b.cpu_model, "run {} ran on {} {} ({}), run 1 on {} {} ({}): a pooled interval would measure the machines", n + 1, b.os, b.arch, b.cpu_model.as_deref().unwrap_or("unknown CPU"), a.os, a.arch, a.cpu_model.as_deref().unwrap_or("unknown CPU"));
		ensure!(report.results.len() == first.results.len(), "run {} holds {} results, run 1 holds {}", n + 1, report.results.len(), first.results.len());
		for (x, y) in first.results.iter().zip(&report.results) {
			ensure!((x.profile.as_str(), x.adapter.as_str(), x.workload.as_str()) == (y.profile.as_str(), y.adapter.as_str(), y.workload.as_str()), "run {} has {} / {} / {} where run 1 has {} / {} / {}", n + 1, y.profile, y.adapter, y.workload, x.profile, x.adapter, x.workload);
			ensure!(x.dataset.input_points == y.dataset.input_points && x.dataset.seed == y.dataset.seed, "{} / {}: run {} measured {} points from seed {}, run 1 {} from seed {}", x.profile, x.adapter, n + 1, y.dataset.input_points, y.dataset.seed, x.dataset.input_points, x.dataset.seed);
		}
	}
	let results = (0..first.results.len()).map(|i| combine_result(&reports.iter().map(|report| &report.results[i]).collect::<Vec<_>>())).collect::<anyhow::Result<Vec<_>>>()?;
	Ok(BenchReport::with_results(first.metadata.clone(), results))
}

/// Pool one result's runs.
fn combine_result(runs: &[&BenchResult]) -> anyhow::Result<BenchResult> {
	for (n, run) in runs.iter().enumerate() {
		if run.samples_ns.is_empty() {
			bail!("{} / {}: run {} carries no raw samples (written before report schema v28), so its spread cannot be pooled", run.profile, run.adapter, n + 1);
		}
	}
	let mut by_p50: Vec<usize> = (0..runs.len()).collect();
	by_p50.sort_by_key(|&i| runs[i].latency.p50_ns);
	let mut combined = runs[by_p50[runs.len() / 2]].clone();

	let samples: Vec<Vec<u64>> = runs.iter().map(|run| run.samples_ns.clone()).collect();
	combined.samples_ns = samples.concat();
	combined.latency = LatencyStats::from_samples(&combined.samples_ns);
	combined.latency_ci = runs.iter().find_map(|run| run.latency_ci.as_ref()).map(|ci| LatencyStats::clustered_bootstrap_cis(&samples, &BootstrapConfig { resamples: ci.resamples, confidence: ci.confidence, seed: ci.seed }));
	combined.reps = runs.iter().map(|run| run.reps).sum();
	combined.timing.dataset_generation_ns = runs.iter().map(|run| run.timing.dataset_generation_ns).fold(0, u64::saturating_add);
	combined.timing.measured_ns = runs.iter().map(|run| run.timing.measured_ns).fold(0, u64::saturating_add);
	combined.timing.end_to_end_ns = runs.iter().map(|run| run.timing.end_to_end_ns).fold(0, u64::saturating_add);

	let digests_agree = runs.windows(2).all(|pair| pair[0].correctness.result_sha256 == pair[1].correctness.result_sha256);
	combined.correctness.output_count_ok = runs.iter().all(|run| run.correctness.output_count_ok) && digests_agree;
	combined.correctness.values_finite = runs.iter().all(|run| run.correctness.values_finite);
	if !digests_agree {
		combined.correctness.result_sha256 = None;
	}
	let run_p50_ns: Vec<u64> = runs.iter().map(|run| run.latency.p50_ns).collect();
	let (fastest, slowest) = (run_p50_ns.iter().copied().min().unwrap_or(0), run_p50_ns.iter().copied().max().unwrap_or(0));
	let p50_spread = if fastest > 0 { crate::num::u64_to_f64(slowest) / crate::num::u64_to_f64(fastest) - 1.0 } else { 0.0 };
	combined.between_run = Some(BetweenRun { runs: runs.len(), run_p50_ns, run_mean_ns: runs.iter().map(|run| run.latency.mean_ns).collect(), p50_spread, digests_agree });
	Ok(combined)
}

/// Read the reports at `paths`, in order.
///
/// # Errors
///
/// A path that cannot be read or does not hold a report.
pub fn read_reports(paths: &[std::path::PathBuf]) -> anyhow::Result<Vec<BenchReport>> {
	paths.iter().map(|path| std::fs::read(path).with_context(|| format!("cannot read {}", path.display())).and_then(|bytes| serde_json::from_slice(&bytes).with_context(|| format!("{} is not a weft-bench report", path.display())))).collect()
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		report::RunMetadata, schema::{CorrectnessReport, DatasetMeta, TimingBreakdown, SCHEMA_VERSION}
	};

	fn run(level: u64, digest: &str) -> BenchReport {
		let samples: Vec<u64> = (0..20).map(|i| level + i % 4).collect();
		let result = BenchResult { schema_version: SCHEMA_VERSION, profile: "p".into(), adapter: "weftdb".into(), workload: "w".into(), reps: samples.len(), dataset: DatasetMeta { input_points: 100, output_points: 10, irregular: false, missingness_fraction: 0.0, seed: 7, signal_shape: None }, latency: LatencyStats::from_samples(&samples), latency_ci: Some(LatencyStats::bootstrap_cis(&samples, &BootstrapConfig::default())), throughput_points_per_sec: 1.0, timing: TimingBreakdown { dataset_generation_ns: 1, measured_ns: samples.iter().sum(), end_to_end_ns: 2 }, correctness: CorrectnessReport { output_count_ok: true, expected_output_points: 10, actual_output_points: 10, values_finite: true, result_sha256: Some(digest.into()) }, accuracy: None, storage: None, cold_warm: None, gpu_pool: None, adapter_info: None, growth: None, online: None, compressed_query: None, analytics: None, samples_ns: samples, between_run: None, gpu_economics: None };
		BenchReport::with_results(RunMetadata::capture("2026-11-02T00:00:00Z".into()), vec![result])
	}

	#[test]
	fn runs_pool_into_one_result_whose_interval_spans_them() {
		let runs = [run(1_000, "a"), run(1_300, "a"), run(1_100, "a")];
		let combined = combine(&runs).expect("combines");
		let result = &combined.results[0];
		assert_eq!((result.reps, result.samples_ns.len()), (60, 60));
		assert_eq!(result.latency, LatencyStats::from_samples(&runs.iter().flat_map(|r| r.results[0].samples_ns.clone()).collect::<Vec<_>>()));
		let between = result.between_run.as_ref().expect("between-run figures");
		assert_eq!(between.run_p50_ns, vec![1_001, 1_301, 1_101]);
		assert!((between.p50_spread - 0.3).abs() < 0.01, "{}", between.p50_spread);
		assert!(between.digests_agree && result.is_publishable());
		// Each run alone is sure of its median to a few ns; pooled, the interval reaches across runs.
		let ci = result.latency_ci.as_ref().expect("intervals");
		let alone = runs[0].results[0].latency_ci.as_ref().expect("intervals");
		assert!(ci.p50.upper_ns - ci.p50.lower_ns > 100 && alone.p50.upper_ns - alone.p50.lower_ns < 10, "{:?} vs {:?}", ci.p50, alone.p50);
		assert_eq!(result.timing.measured_ns, runs.iter().map(|r| r.results[0].timing.measured_ns).sum::<u64>());
	}

	#[test]
	fn a_run_that_answered_differently_fails_the_combined_result() {
		let combined = combine(&[run(1_000, "a"), run(1_000, "b")]).expect("combines");
		let result = &combined.results[0];
		assert!(!result.is_publishable());
		assert!(!result.between_run.as_ref().expect("figures").digests_agree);
		assert_eq!(result.correctness.result_sha256, None);
	}

	#[test]
	fn only_runs_of_the_same_benchmark_combine() {
		assert!(combine(&[run(1_000, "a")]).is_err(), "one run is not a combination");
		let mut other_seed = run(1_000, "a");
		other_seed.results[0].dataset.seed = 8;
		assert!(combine(&[run(1_000, "a"), other_seed]).is_err());
		let mut other_cpu = run(1_000, "a");
		other_cpu.metadata.cpu_model = Some("another CPU".into());
		assert!(combine(&[run(1_000, "a"), other_cpu]).is_err());
		let mut no_samples = run(1_000, "a");
		no_samples.results[0].samples_ns.clear();
		let err = combine(&[run(1_000, "a"), no_samples]).expect_err("needs samples").to_string();
		assert!(err.contains("schema v28"), "{err}");
	}
}
