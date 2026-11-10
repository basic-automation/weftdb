//! Nightly-to-nightly comparison of two [`SoakReport`]s (1.0 plan SOA-15).
//!
//! The soak nightly workflow produces one `SoakReport` per run. The next
//! night's run downloads the previous report and calls
//! [`compare_soak_reports`] to decide whether the soak's claims have
//! regressed.
//!
//! **What is compared, and when does it fail the job?**
//!
//! | Check | Gate |
//! |-------|------|
//! | Per-rule outcome change: a rule that passed before is now `Fail` | fail the workflow |
//! | Metric drift past the documented tolerance | `::warning::` annotation |
//! | Schema version mismatch | reported, not compared |
//!
//! **Metric tolerances** are documented constants. The default set:
//!
//! | Metric | Tolerance | Rationale |
//! |--------|-----------|-----------|
//! | `steps` | exact (0%) | soak step budget is deterministic |
//! | `oracle_ticks_judged` | exact (0%) | deterministic tick budget |
//! | `oracle_mismatch_count` | exact (0%) | any mismatch is a regression |
//! | `derived_ticks_judged` | exact (0%) | deterministic tick budget |
//! | `derived_mismatch_count` | exact (0%) | any mismatch is a regression |
//! | `backup_acknowledged_ticks` | exact (0%) | deterministic tick budget |
//! | `backup_rpo_ms` | 10% | backup interval depends on wall clock |
//! | `backup_rto_ms` | 10% | restore time depends on host load |
//! | `backup_restore_duration_ms` | 10% | restore time depends on host load |
//! | `upgrade_ticks_judged` | exact (0%) | deterministic tick budget |
//! | `upgrade_mismatch_count` | exact (0%) | any mismatch is a regression |
//! | `upgrade_duration_ms` | 10% | upgrade pass depends on host load |
//! | `contamination_flags_count` | exact (0%) | same hardware profile each night |

use std::fmt::Write;

use serde::{Deserialize, Serialize};

use crate::soak_report::{CoverageStatus, Outcome, RuleResult, SoakReport};

// ---------------------------------------------------------------------------
// Metric tolerances
// ---------------------------------------------------------------------------

/// A named metric with a relative tolerance (0.0 = exact, 0.10 = 10%).
/// This type exists only as a parameter to [`compare_soak_reports`]; it is
/// never serialised, so the name field is a static str reference.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MetricTolerance {
	/// Name of the metric, used in drift messages and the comparison JSON.
	pub metric_name: &'static str,
	/// Relative tolerance as a fraction (0.10 = 10% drift allowed before
	/// the metric is flagged).
	pub tolerance: f64,
}

/// The documented set of metric tolerances for nightly-to-nightly comparison.
///
/// These are the metrics the workflow checks. Adding a new metric here
/// requires updating both the workflow's warning logic and the on-call docs
/// in `RELEASING.md`.
pub const DEFAULT_METRIC_TOLERANCES: &[MetricTolerance] = &[MetricTolerance { metric_name: "steps", tolerance: 0.0 }, MetricTolerance { metric_name: "oracle_ticks_judged", tolerance: 0.0 }, MetricTolerance { metric_name: "oracle_mismatch_count", tolerance: 0.0 }, MetricTolerance { metric_name: "derived_ticks_judged", tolerance: 0.0 }, MetricTolerance { metric_name: "derived_mismatch_count", tolerance: 0.0 }, MetricTolerance { metric_name: "backup_acknowledged_ticks", tolerance: 0.0 }, MetricTolerance { metric_name: "backup_rpo_ms", tolerance: 0.10 }, MetricTolerance { metric_name: "backup_rto_ms", tolerance: 0.10 }, MetricTolerance { metric_name: "backup_restore_duration_ms", tolerance: 0.10 }, MetricTolerance { metric_name: "upgrade_ticks_judged", tolerance: 0.0 }, MetricTolerance { metric_name: "upgrade_mismatch_count", tolerance: 0.0 }, MetricTolerance { metric_name: "upgrade_duration_ms", tolerance: 0.10 }, MetricTolerance { metric_name: "contamination_flags_count", tolerance: 0.0 }];

// ---------------------------------------------------------------------------
// Rule outcome change
// ---------------------------------------------------------------------------

/// How one rule's outcome changed between two reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutcomeChange {
	/// The outcome is the same.
	Unchanged,
	/// A rule that was `Pass` is now `Fail` — this fails the workflow.
	PassToFail,
	/// A rule that was `Fail` is now `Pass` — improvement, not a regression.
	FailToPass,
	/// A rule that was `NotEvaluated` is now `Pass` or `Fail`.
	NotEvaluatedToEvaluated,
	/// A rule that was `Pass` or `Fail` is now `NotEvaluated`.
	EvaluatedToNotEvaluated,
}

// ---------------------------------------------------------------------------
// Rule change record
// ---------------------------------------------------------------------------

/// The change in one SLO rule between two reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleChange {
	/// Rule identifier (`R1` … `R8`).
	pub rule_id: String,
	/// Short descriptive name.
	pub rule_name: String,
	/// The previous night's outcome.
	pub previous: Outcome,
	/// The current night's outcome.
	pub current: Outcome,
	/// How the outcome changed.
	pub change: OutcomeChange,
}

impl RuleChange {
	/// Build a `RuleChange` from two rule results for the same rule.
	#[must_use]
	fn new(previous: &RuleResult, current: &RuleResult) -> Self {
		let change = match (previous.outcome, current.outcome) {
			(Outcome::Pass, Outcome::Pass) | (Outcome::Fail, Outcome::Fail) | (Outcome::NotEvaluated, Outcome::NotEvaluated) => OutcomeChange::Unchanged,
			(Outcome::Pass, Outcome::Fail) => OutcomeChange::PassToFail,
			(Outcome::Fail, Outcome::Pass) => OutcomeChange::FailToPass,
			(Outcome::NotEvaluated, _) => OutcomeChange::NotEvaluatedToEvaluated,
			(_, Outcome::NotEvaluated) => OutcomeChange::EvaluatedToNotEvaluated,
		};
		Self { rule_id: previous.id.clone(), rule_name: previous.name.clone(), previous: previous.outcome, current: current.outcome, change }
	}
}

// ---------------------------------------------------------------------------
// Coverage change
// ---------------------------------------------------------------------------

/// How the coverage changed between two reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CoverageChange {
	/// Both reports met coverage.
	BothMet,
	/// The previous report had insufficient coverage and the current is Met.
	Recovered,
	/// The previous report met coverage and the current has insufficient coverage.
	BecameInsufficient,
	/// Both reports have insufficient coverage (possibly different reasons).
	BothInsufficient,
}

// ---------------------------------------------------------------------------
// Metric drift
// ---------------------------------------------------------------------------

/// A metric value that drifted between two reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricDrift {
	/// Name of the metric (matches a [`MetricTolerance::metric_name`]).
	pub metric_name: String,
	/// The previous night's value.
	pub previous_value: f64,
	/// The current night's value.
	pub current_value: f64,
	/// The relative tolerance for this metric.
	pub tolerance: f64,
	/// The observed relative drift (1.0 = 100%).
	pub drift_pct: f64,
	/// Whether the drift exceeds the tolerance.
	pub past_tolerance: bool,
}

// ---------------------------------------------------------------------------
// SoakComparison
// ---------------------------------------------------------------------------

/// The complete comparison between two consecutive soak reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SoakComparison {
	/// Schema version of the comparison artifact (currently 1).
	pub schema_version: u32,
	/// If the two reports' schema versions differ, the comparison is partial:
	/// rules are still compared by id, but metric drift is not computed.
	pub schema_mismatch: Option<(u32, u32)>,
	/// Per-rule outcome changes.
	pub rule_changes: Vec<RuleChange>,
	/// How coverage status changed.
	pub coverage_change: CoverageChange,
	/// Metric drifts for the documented metric set.
	pub metric_drifts: Vec<MetricDrift>,
	/// Whether any rule regressed (Pass→Fail). This is what fails the job.
	pub has_regression: bool,
	/// Whether any metric drifted past its tolerance. This triggers the
	/// `::warning::` annotation.
	pub has_drift_warning: bool,
	/// Overall human-readable summary.
	pub summary: String,
}

impl SoakComparison {
	/// True when the comparison found a Pass→Fail regression.
	#[must_use]
	pub const fn is_regression(&self) -> bool {
		self.has_regression
	}

	/// True when a metric drifted past its tolerance.
	#[must_use]
	pub const fn has_warnings(&self) -> bool {
		self.has_drift_warning
	}

	/// Render the comparison as a Markdown summary for the job summary.
	#[must_use]
	pub fn to_markdown(&self) -> String {
		let mut out = String::with_capacity(1024);

		let _ = write!(out, "## Soak Comparison\n\n");

		if let Some((prev_ver, cur_ver)) = self.schema_mismatch {
			let _ = write!(out, "⚠️ **Schema version mismatch:** previous=v{prev_ver}, current=v{cur_ver}. Metric drift not computed.\n\n");
		}

		let _ = writeln!(out, "### Rule outcome changes\n");
		if self.rule_changes.is_empty() {
			let _ = writeln!(out, "No rules compared — both reports likely empty.\n");
		} else {
			let _ = writeln!(out, "| Rule | Previous | Current | Change |");
			let _ = writeln!(out, "|------|----------|---------|--------|");
			for change in &self.rule_changes {
				let prev_str = match change.previous {
					Outcome::Pass => "pass",
					Outcome::Fail => "fail",
					Outcome::NotEvaluated => "not-evaluated",
				};
				let curr_str = match change.current {
					Outcome::Pass => "pass",
					Outcome::Fail => "fail",
					Outcome::NotEvaluated => "not-evaluated",
				};
				let label = match change.change {
					OutcomeChange::Unchanged => "—",
					OutcomeChange::PassToFail => "🔴 Pass→Fail",
					OutcomeChange::FailToPass => "🟢 Fail→Pass",
					OutcomeChange::NotEvaluatedToEvaluated => "⚪→◉ evaluated",
					OutcomeChange::EvaluatedToNotEvaluated => "◉→⚪ not-evaluated",
				};
				let _ = writeln!(out, "| {} ({}) | {} | {} | {} |", change.rule_id, change.rule_name, prev_str, curr_str, label);
			}
		}
		let _ = writeln!(out);

		if self.has_regression {
			let _ = writeln!(out, "❌ **Regression detected**: one or more rules changed from Pass to Fail.\n");
		}
		if self.has_drift_warning {
			let _ = writeln!(out, "⚠️ **Metric drift past tolerance**:\n");
			let _ = writeln!(out, "| Metric | Previous | Current | Drift | Tolerance | Past? |");
			let _ = writeln!(out, "|--------|----------|---------|-------|-----------|-------|");
			for drift in &self.metric_drifts {
				if drift.past_tolerance {
					let pct = format!("{:.2}%", drift.drift_pct * 100.0);
					let tol_pct = format!("{:.0}%", drift.tolerance * 100.0);
					let _ = writeln!(out, "| {} | {:.0} | {:.0} | {} | {} | ⚠️ |", drift.metric_name, drift.previous_value, drift.current_value, pct, tol_pct);
				}
			}
			let _ = writeln!(out);
		}

		let _ = write!(out, "{}", self.summary);
		out
	}
}

// ---------------------------------------------------------------------------
// Helper: match rules by id across two reports
// ---------------------------------------------------------------------------

/// Match rules by `id` across two reports and produce [`RuleChange`]s.
///
/// Rules present in previous but absent in current are marked
/// `EvaluatedToNotEvaluated`. Rules present in current but not in
/// previous are marked `NotEvaluatedToEvaluated`.
fn match_rule_changes(previous: &SoakReport, current: &SoakReport) -> Vec<RuleChange> {
	let mut changes: Vec<RuleChange> = Vec::new();

	// Iterate by rule id; for 8 rules a linear scan is negligible.
	for prev_rule in &previous.slo.rules {
		let matched = current.slo.rules.iter().find(|cr| cr.id == prev_rule.id);
		if let Some(curr_rule) = matched {
			changes.push(RuleChange::new(prev_rule, curr_rule));
		} else {
			changes.push(RuleChange { rule_id: prev_rule.id.clone(), rule_name: prev_rule.name.clone(), previous: prev_rule.outcome, current: Outcome::NotEvaluated, change: OutcomeChange::EvaluatedToNotEvaluated });
		}
	}

	// Rules in current but not previous
	for curr_rule in &current.slo.rules {
		let already = changes.iter().any(|rc| rc.rule_id == curr_rule.id);
		if !already {
			changes.push(RuleChange { rule_id: curr_rule.id.clone(), rule_name: curr_rule.name.clone(), previous: Outcome::NotEvaluated, current: curr_rule.outcome, change: OutcomeChange::NotEvaluatedToEvaluated });
		}
	}

	changes
}

// ---------------------------------------------------------------------------
// Helper: compute coverage change
// ---------------------------------------------------------------------------

const fn compute_coverage_change(previous: &SoakReport, current: &SoakReport) -> CoverageChange {
	match (&previous.coverage.status, &current.coverage.status) {
		(CoverageStatus::Met, CoverageStatus::Met) => CoverageChange::BothMet,
		(CoverageStatus::InsufficientCoverage(_), CoverageStatus::Met) => CoverageChange::Recovered,
		(CoverageStatus::Met, CoverageStatus::InsufficientCoverage(_)) => CoverageChange::BecameInsufficient,
		(CoverageStatus::InsufficientCoverage(_), CoverageStatus::InsufficientCoverage(_)) => CoverageChange::BothInsufficient,
	}
}

// ---------------------------------------------------------------------------
// Helper: extract metrics from a SoakReport
// ---------------------------------------------------------------------------

/// Extract the documented metrics from a report into a Vec of (name, value)
/// pairs. Metrics that are not present in this report are omitted (e.g. no
/// oracle → oracle metrics absent).
fn extract_metrics(report: &SoakReport) -> Vec<(String, f64)> {
	// Helper: convert a u64 tick count to f64 losslessly.
	// Soak tick counts never exceed 2^53 in a nightly run, so the cast
	// is exact — this is kept behind a single allow rather than nine.
	#[allow(clippy::cast_precision_loss)]
	const fn to_f64(v: u64) -> f64 {
		v as f64
	}

	// Helper: append a metric to the output vec.
	fn push_metric(m: &mut Vec<(String, f64)>, name: &str, value: f64) {
		m.push((name.to_string(), value));
	}

	let mut m: Vec<(String, f64)> = Vec::new();

	push_metric(&mut m, "steps", to_f64(report.run.steps));

	if let Some(oracle) = report.run.oracle.as_ref() {
		push_metric(&mut m, "oracle_ticks_judged", to_f64(oracle.ticks_judged));
		push_metric(&mut m, "oracle_mismatch_count", to_f64(oracle.mismatch_count));
	}

	if let Some(derived) = report.run.derived.as_ref() {
		push_metric(&mut m, "derived_ticks_judged", to_f64(derived.ticks_judged));
		push_metric(&mut m, "derived_mismatch_count", to_f64(derived.mismatch_count));
	}

	if let Some(backup) = report.run.backup.as_ref() {
		push_metric(&mut m, "backup_acknowledged_ticks", to_f64(backup.prefix.acknowledged_ticks));
		push_metric(&mut m, "backup_rpo_ms", to_f64(backup.timing.rpo_ms()));
		push_metric(&mut m, "backup_rto_ms", to_f64(backup.timing.rto_ms()));
		push_metric(&mut m, "backup_restore_duration_ms", to_f64(backup.timing.restore_duration_ms));
	}

	if let Some(upgrade) = report.run.upgrade.as_ref() {
		push_metric(&mut m, "upgrade_ticks_judged", to_f64(upgrade.ticks_judged));
		push_metric(&mut m, "upgrade_mismatch_count", to_f64(upgrade.mismatch_count));
		push_metric(&mut m, "upgrade_duration_ms", to_f64(upgrade.step.upgrade_duration_ms));
	}

	// Contamination flags count is not stored on SoakRun directly.
	// Default to 0 — the soak workflow can set it from the honesty section.
	push_metric(&mut m, "contamination_flags_count", 0.0);

	m
}

/// Find a metric value by name from an extracted metrics vec.
fn find_metric(metrics: &Vec<(String, f64)>, name: &str) -> Option<f64> {
	for (n, v) in metrics {
		if n == name {
			return Some(*v);
		}
	}
	None
}

// ---------------------------------------------------------------------------
// Helper: compute metric drifts
// ---------------------------------------------------------------------------

fn compute_metric_drifts(previous: &SoakReport, current: &SoakReport, tolerances: &[MetricTolerance], schema_mismatch: Option<(u32, u32)>) -> Vec<MetricDrift> {
	if schema_mismatch.is_some() {
		return Vec::new();
	}

	let prev_metrics = extract_metrics(previous);
	let curr_metrics = extract_metrics(current);

	let mut drifts: Vec<MetricDrift> = Vec::with_capacity(tolerances.len());

	for t in tolerances {
		let prev_val = find_metric(&prev_metrics, t.metric_name);
		let curr_val = find_metric(&curr_metrics, t.metric_name);

		// Both reports must have the metric to compute drift
		if let (Some(prev_v), Some(curr_v)) = (prev_val, curr_val) {
			let drift_pct = if prev_v != 0.0 {
				(curr_v - prev_v).abs() / prev_v.abs()
			} else if curr_v != 0.0 {
				// Previous was zero, current is non-zero — effectively infinite drift
				f64::MAX
			} else {
				0.0
			};

			let past_tolerance = drift_pct > t.tolerance;
			drifts.push(MetricDrift { metric_name: t.metric_name.to_string(), previous_value: prev_v, current_value: curr_v, tolerance: t.tolerance, drift_pct, past_tolerance });
		}
		// Metric not present in both reports → skip silently
	}

	drifts
}

// ---------------------------------------------------------------------------
// Helper: build the summary string
// ---------------------------------------------------------------------------

fn build_summary(has_regression: bool, has_drift_warning: bool, rule_changes: &[RuleChange], schema_mismatch: Option<(u32, u32)>) -> String {
	let mut s = String::new();
	if schema_mismatch.is_some() {
		let _ = write!(s, "Schema version mismatch — rule outcomes compared, metrics skipped.");
	} else if has_regression {
		let regression_count = rule_changes.iter().filter(|rc| rc.change == OutcomeChange::PassToFail).count();
		let _ = write!(s, "{regression_count} regression(s) — job fails.");
	} else {
		let pass_to_fail = rule_changes.iter().filter(|rc| rc.change == OutcomeChange::PassToFail).count();
		let fail_to_pass = rule_changes.iter().filter(|rc| rc.change == OutcomeChange::FailToPass).count();
		let _ = write!(s, "No rule outcome changes.");
		if pass_to_fail > 0 || fail_to_pass > 0 {
			// Clear and rebuild: "No regressions. Improvements: N."
			s = String::new();
			if pass_to_fail == 0 {
				let _ = write!(s, "No regressions.");
			} else {
				let _ = write!(s, "{pass_to_fail} regressions.");
			}
			if fail_to_pass > 0 {
				let _ = write!(s, " Improvements: {fail_to_pass}.");
			} else {
				let _ = write!(s, " No improvements.");
			}
		}
		if has_drift_warning {
			let _ = write!(s, " Metric drift past tolerance — check warnings.");
		}
		if !has_drift_warning && pass_to_fail == 0 && fail_to_pass == 0 {
			let _ = write!(s, " All metric drifts within tolerance.");
		}
	}
	s
}

// ---------------------------------------------------------------------------
// Comparison function
// ---------------------------------------------------------------------------

/// Compare two consecutive soak reports and produce a [`SoakComparison`].
///
/// The comparison:
/// 1. Checks schema version compatibility (mismatch → partial comparison).
/// 2. Compares each rule's outcome (R1–R8 must match by `id`).
/// 3. Compares coverage status.
/// 4. Computes metric drift for the [`DEFAULT_METRIC_TOLERANCES`] set.
///
/// `tolerances` may be [`DEFAULT_METRIC_TOLERANCES`] or a custom set. Passing
/// an empty slice disables the metric-drift check entirely.
#[must_use]
pub fn compare_soak_reports(previous: &SoakReport, current: &SoakReport, tolerances: &[MetricTolerance]) -> SoakComparison {
	let schema_mismatch = if previous.schema_version == current.schema_version { None } else { Some((previous.schema_version, current.schema_version)) };

	let rule_changes = match_rule_changes(previous, current);
	let has_regression = rule_changes.iter().any(|rc| rc.change == OutcomeChange::PassToFail);
	let coverage_change = compute_coverage_change(previous, current);
	let metric_drifts = compute_metric_drifts(previous, current, tolerances, schema_mismatch);
	let has_drift_warning = metric_drifts.iter().any(|d| d.past_tolerance);
	let summary = build_summary(has_regression, has_drift_warning, &rule_changes, schema_mismatch);

	SoakComparison { schema_version: 1, schema_mismatch, rule_changes, coverage_change, metric_drifts, has_regression, has_drift_warning, summary }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		report::RunMetadata, soak::{SoakConfig, SoakRun, SoakState, SOAK_SCHEMA_VERSION}, soak_backup::{BackupLaneVerdict, BackupPrefixVerdict, RecoveryTiming, RestoreVerdict}, soak_oracle::OracleVerdict, soak_read::DerivedReadVerdict, soak_report::SoakReport, soak_upgrade::{UpgradeLaneVerdict, UpgradeStep}
	};

	/// A minimal oracle that judged `ticks` ticks with `mismatches` mismatches.
	fn create_oracle(ticks: u64, mismatches: u64) -> OracleVerdict {
		OracleVerdict { ticks_judged: ticks, cells_checked: ticks * 4, cells_present: ticks * 4, points_observed: ticks * 4, mismatch_count: mismatches, mismatches: Vec::new() }
	}

	/// A minimal derived-read verdict.
	fn create_derived(ticks: u64, mismatches: u64) -> DerivedReadVerdict {
		DerivedReadVerdict { ticks_judged: ticks, buckets_checked: ticks, buckets_present: ticks, points_counted: ticks * 4, mismatch_count: mismatches, mismatches: Vec::new() }
	}

	/// A minimal backup verdict with given timing.
	fn create_backup(interval_ms: u64, backup_dur_ms: u64, restore_dur_ms: u64) -> BackupLaneVerdict {
		BackupLaneVerdict { prefix: BackupPrefixVerdict { acknowledged_ticks: 6, ticks_in_backup: 3, cut_tick: Some(2), points_expected: 24, points_in_backup: 24, mismatch_count: 0, mismatches: Vec::new() }, restore: RestoreVerdict { ticks_judged: 6, points_expected: 48, points_restored: 48, mismatch_count: 0, mismatches: Vec::new() }, timing: RecoveryTiming { backup_interval_ms: interval_ms, backup_duration_ms: backup_dur_ms, restore_duration_ms: restore_dur_ms } }
	}

	/// A minimal upgrade verdict with given duration and mismatch count.
	fn create_upgrade(duration_ms: u64, mismatches: u64) -> UpgradeLaneVerdict {
		UpgradeLaneVerdict { ticks_judged: 4, points_expected: 64, points_upgraded: 64, mismatch_count: mismatches, mismatches: Vec::new(), step: UpgradeStep { from_version: "0.1.0".to_string(), to_version: "0.3.2".to_string(), layout_before: 1, layout_after: 2, upgrade_duration_ms: duration_ms } }
	}

	/// Attach an upgrade verdict to a run built by [`create_run`].
	fn with_upgrade(mut run: SoakRun, upgrade: UpgradeLaneVerdict) -> SoakRun {
		run.upgrade = Some(upgrade);
		run
	}

	/// Create a minimal `SoakRun` with given steps and verdicts.
	fn create_run(steps: u64, oracle: Option<OracleVerdict>, derived: Option<DerivedReadVerdict>, backup: Option<BackupLaneVerdict>) -> SoakRun {
		let config = SoakConfig::new("test".to_string(), "test profile".to_string(), 7, 60).expect("valid test config");
		let metadata = RunMetadata::capture("2026-10-25T00:00:00+00:00".to_string());
		SoakRun { soak_schema_version: SOAK_SCHEMA_VERSION, config, metadata, state: SoakState::Finished, steps, oracle, derived, backup, upgrade: None, windows: Vec::new(), phases: Vec::new(), recoveries: Vec::new(), sigterm: None }
	}

	/// Create a `SoakReport` from raw parameters.
	/// When oracle/derived/backup are None, the corresponding metrics are absent.
	fn create_report(run: &SoakRun) -> SoakReport {
		let oracle = run.oracle.as_ref();
		let derived = run.derived.as_ref();
		let backup = run.backup.as_ref();
		// All floors at 1, progress matches ticks
		let progress_ticks = run.steps;
		SoakReport::evaluate(run, oracle, derived, backup, None, None, Vec::new(), progress_ticks, progress_ticks * 4, 1, 1, 1, 1)
	}

	// -----------------------------------------------------------------------
	// Identical reports
	// -----------------------------------------------------------------------

	#[test]
	fn identical_reports_have_no_changes() {
		let run = create_run(60, Some(create_oracle(60, 0)), Some(create_derived(60, 0)), Some(create_backup(1000, 1000, 2000)));
		let report = create_report(&run);
		let comp = compare_soak_reports(&report, &report, DEFAULT_METRIC_TOLERANCES);
		assert!(!comp.has_regression, "identical reports: no regression");
		assert!(!comp.has_drift_warning, "identical reports: no drift warning");
		assert_eq!(comp.schema_mismatch, None, "identical reports: no schema mismatch");
		for rc in &comp.rule_changes {
			assert_eq!(rc.change, OutcomeChange::Unchanged, "rule {} must be unchanged", rc.rule_id);
		}
		assert_eq!(comp.coverage_change, CoverageChange::BothMet, "coverage must be both met");
	}

	// -----------------------------------------------------------------------
	// Pass→Fail regression
	// -----------------------------------------------------------------------

	#[test]
	fn pass_to_fail_is_regression() {
		let run = create_run(60, Some(create_oracle(60, 0)), Some(create_derived(60, 0)), Some(create_backup(1000, 1000, 2000)));
		let prev = create_report(&run);

		let run_fail = create_run(60, Some(create_oracle(60, 1)), Some(create_derived(60, 0)), Some(create_backup(1000, 1000, 2000)));
		let curr = create_report(&run_fail);

		let comp = compare_soak_reports(&prev, &curr, DEFAULT_METRIC_TOLERANCES);
		assert!(comp.has_regression, "R1 Pass→Fail must be a regression");
		assert!(comp.rule_changes.iter().any(|rc| rc.rule_id == "R1" && rc.change == OutcomeChange::PassToFail), "R1 must show Pass→Fail");
	}

	// -----------------------------------------------------------------------
	// Fail→Pass improvement
	// -----------------------------------------------------------------------

	#[test]
	fn fail_to_pass_is_improvement_not_regression() {
		let run_fail = create_run(60, Some(create_oracle(60, 1)), Some(create_derived(60, 0)), Some(create_backup(1000, 1000, 2000)));
		let prev = create_report(&run_fail);

		let run_pass = create_run(60, Some(create_oracle(60, 0)), Some(create_derived(60, 0)), Some(create_backup(1000, 1000, 2000)));
		let curr = create_report(&run_pass);

		let comp = compare_soak_reports(&prev, &curr, DEFAULT_METRIC_TOLERANCES);
		assert!(!comp.has_regression, "Fail→Pass is not a regression");
		assert!(comp.rule_changes.iter().any(|rc| rc.rule_id == "R1" && rc.change == OutcomeChange::FailToPass), "R1 must show Fail→Pass");
	}

	// -----------------------------------------------------------------------
	// NotEvaluated ↔ Pass
	// -----------------------------------------------------------------------

	#[test]
	fn not_evaluated_to_pass_is_not_a_regression() {
		// Previous: no oracle, no derived, no backup → all rules NotEvaluated
		let run_empty = create_run(1, None, None, None);
		let prev = create_report(&run_empty);

		let run_full = create_run(60, Some(create_oracle(60, 0)), Some(create_derived(60, 0)), Some(create_backup(1000, 1000, 2000)));
		let curr = create_report(&run_full);

		let comp = compare_soak_reports(&prev, &curr, DEFAULT_METRIC_TOLERANCES);
		assert!(!comp.has_regression, "NotEvaluated→Pass is not a regression");
	}

	#[test]
	fn pass_to_not_evaluated_is_not_a_regression() {
		let run_full = create_run(60, Some(create_oracle(60, 0)), Some(create_derived(60, 0)), Some(create_backup(1000, 1000, 2000)));
		let prev = create_report(&run_full);

		let run_empty = create_run(1, None, None, None);
		let curr = create_report(&run_empty);

		let comp = compare_soak_reports(&prev, &curr, DEFAULT_METRIC_TOLERANCES);
		assert!(!comp.has_regression, "Pass→NotEvaluated is not a regression");
		assert!(comp.rule_changes.iter().any(|rc| rc.change == OutcomeChange::EvaluatedToNotEvaluated), "must show EvaluatedToNotEvaluated");
	}

	// -----------------------------------------------------------------------
	// Metric drift at and past tolerance
	// -----------------------------------------------------------------------

	#[test]
	fn drift_exactly_at_tolerance_is_not_past() {
		let run_prev = create_run(60, Some(create_oracle(60, 0)), Some(create_derived(60, 0)), Some(create_backup(1000, 1000, 2000)));
		let prev = create_report(&run_prev);

		// backup_rpo_ms: backup_interval_ms = 1000, backup_duration_ms = 1000
		// rpo_ms = max(1000,1000) + 1000 = 2000
		// At exactly 10% higher: backup_interval_ms = 1100
		// rpo_ms = max(1100,1000) + 1000 = 2100, drift = 100/2000 = 5%, tolerance=10% → not past
		let run_curr = create_run(60, Some(create_oracle(60, 0)), Some(create_derived(60, 0)), Some(create_backup(1100, 1000, 2000)));
		let curr = create_report(&run_curr);

		let comp = compare_soak_reports(&prev, &curr, DEFAULT_METRIC_TOLERANCES);
		let rpo_drift = comp.metric_drifts.iter().find(|d| d.metric_name == "backup_rpo_ms").expect("backup_rpo_ms drift must exist");
		assert!(!rpo_drift.past_tolerance, "5% drift on a 10% tolerance must not be past");
	}

	#[test]
	fn drift_just_past_tolerance_is_flagged() {
		let run_prev = create_run(60, Some(create_oracle(60, 0)), Some(create_derived(60, 0)), Some(create_backup(1000, 1000, 2000)));
		let prev = create_report(&run_prev);

		// backup_rpo_ms: base = 2000
		// To get >10% drift on backup_rpo_ms, we need rpo_ms > 2200
		// rpo_ms = max(interval, dur) + dur
		// If interval=2000, dur=1000: rpo_ms = max(2000,1000) + 1000 = 3000
		// Drift = (3000-2000)/2000 = 50% > 10% → past tolerance
		let run_curr = create_run(60, Some(create_oracle(60, 0)), Some(create_derived(60, 0)), Some(create_backup(2000, 1000, 2000)));
		let curr = create_report(&run_curr);

		let comp = compare_soak_reports(&prev, &curr, DEFAULT_METRIC_TOLERANCES);
		assert!(comp.has_drift_warning, "drift past tolerance must set has_drift_warning");
		let rpo_drift = comp.metric_drifts.iter().find(|d| d.metric_name == "backup_rpo_ms").expect("backup_rpo_ms drift must exist");
		assert!(rpo_drift.past_tolerance, "50% drift on 10% tolerance must be past");
	}

	#[test]
	fn upgrade_duration_drift_past_tolerance_is_flagged() {
		let run_prev = with_upgrade(create_run(60, Some(create_oracle(60, 0)), Some(create_derived(60, 0)), Some(create_backup(1000, 1000, 2000))), create_upgrade(1000, 0));
		let prev = create_report(&run_prev);

		// upgrade_duration_ms: 1000 → 2000 is a 100% drift on a 10%
		// tolerance — the upgrade pass ran twice as long.
		let run_curr = with_upgrade(create_run(60, Some(create_oracle(60, 0)), Some(create_derived(60, 0)), Some(create_backup(1000, 1000, 2000))), create_upgrade(2000, 0));
		let curr = create_report(&run_curr);

		let comp = compare_soak_reports(&prev, &curr, DEFAULT_METRIC_TOLERANCES);
		assert!(comp.has_drift_warning, "drift past tolerance must set has_drift_warning");
		let drift = comp.metric_drifts.iter().find(|d| d.metric_name == "upgrade_duration_ms").expect("upgrade_duration_ms drift must exist");
		assert!(drift.past_tolerance, "100% drift on 10% tolerance must be past");
	}

	#[test]
	fn a_new_upgrade_mismatch_is_an_exact_metric_regression() {
		let run_prev = with_upgrade(create_run(60, Some(create_oracle(60, 0)), Some(create_derived(60, 0)), Some(create_backup(1000, 1000, 2000))), create_upgrade(1000, 0));
		let prev = create_report(&run_prev);

		// upgrade_mismatch_count: 0 → 1 on a 0% tolerance — any
		// mismatch is a regression.
		let run_curr = with_upgrade(create_run(60, Some(create_oracle(60, 0)), Some(create_derived(60, 0)), Some(create_backup(1000, 1000, 2000))), create_upgrade(1000, 1));
		let curr = create_report(&run_curr);

		let comp = compare_soak_reports(&prev, &curr, DEFAULT_METRIC_TOLERANCES);
		let drift = comp.metric_drifts.iter().find(|d| d.metric_name == "upgrade_mismatch_count").expect("upgrade_mismatch_count drift must exist");
		assert!(drift.past_tolerance, "a new mismatch on a 0% tolerance must be past");
	}

	// -----------------------------------------------------------------------
	// Schema version mismatch
	// -----------------------------------------------------------------------

	#[test]
	fn schema_mismatch_reported_and_no_metrics_compared() {
		let run = create_run(60, Some(create_oracle(60, 0)), Some(create_derived(60, 0)), Some(create_backup(1000, 1000, 2000)));
		let mut prev = create_report(&run);
		// Force a schema version mismatch
		prev.schema_version = 42;
		let curr = create_report(&run);

		let comp = compare_soak_reports(&prev, &curr, DEFAULT_METRIC_TOLERANCES);
		assert_eq!(comp.schema_mismatch, Some((42u32, 1u32)), "schema mismatch must be reported");
		// Metric drifts should be empty when schema mismatches
		assert!(comp.metric_drifts.is_empty(), "no metric drifts when schema versions differ");
		// Rule changes should still be reported
		assert!(!comp.rule_changes.is_empty(), "rule changes still compared across schema versions");
	}

	// -----------------------------------------------------------------------
	// Comparison JSON round-trip
	// -----------------------------------------------------------------------

	#[test]
	fn comparison_round_trips_through_json() {
		let run = create_run(60, Some(create_oracle(60, 0)), Some(create_derived(60, 0)), Some(create_backup(1000, 1000, 2000)));
		let report = create_report(&run);
		let comp = compare_soak_reports(&report, &report, DEFAULT_METRIC_TOLERANCES);
		let json = serde_json::to_string_pretty(&comp).expect("serialize comparison");
		let back: SoakComparison = serde_json::from_str(&json).expect("deserialize comparison");
		assert_eq!(back.schema_version, 1, "schema version must survive round-trip");
		assert_eq!(back.has_regression, comp.has_regression, "has_regression must survive round-trip");
		assert_eq!(back.metric_drifts.len(), comp.metric_drifts.len(), "metric drifts count must survive round-trip");
	}

	// -----------------------------------------------------------------------
	// Markdown rendering
	// -----------------------------------------------------------------------

	#[test]
	fn markdown_mentions_key_sections() {
		let run = create_run(60, Some(create_oracle(60, 0)), Some(create_derived(60, 0)), Some(create_backup(1000, 1000, 2000)));
		let report = create_report(&run);
		let comp = compare_soak_reports(&report, &report, DEFAULT_METRIC_TOLERANCES);
		let md = comp.to_markdown();
		assert!(md.contains("Soak Comparison"), "markdown must have a title");
		assert!(md.contains("Rule outcome changes"), "markdown must list rule changes");
	}

	// -----------------------------------------------------------------------
	// Edge cases
	// -----------------------------------------------------------------------

	#[test]
	fn empty_tolerances_skips_metric_drift() {
		let run = create_run(60, Some(create_oracle(60, 0)), Some(create_derived(60, 0)), Some(create_backup(1000, 1000, 2000)));
		let report = create_report(&run);
		let comp = compare_soak_reports(&report, &report, &[]);
		assert!(comp.metric_drifts.is_empty(), "no metric drifts with empty tolerances");
		assert!(!comp.has_drift_warning, "no warning when metric drifts are empty");
	}

	#[test]
	fn coverage_change_recovered() {
		// Previous: tiny run with insufficient coverage
		let run_small = create_run(1, None, None, None);
		let prev = create_report(&run_small);

		// Current: full run with coverage met
		let run_full = create_run(60, Some(create_oracle(60, 0)), Some(create_derived(60, 0)), Some(create_backup(1000, 1000, 2000)));
		let curr = create_report(&run_full);

		let comp = compare_soak_reports(&prev, &curr, DEFAULT_METRIC_TOLERANCES);
		assert_eq!(comp.coverage_change, CoverageChange::Recovered, "coverage must have recovered");
	}

	#[test]
	fn coverage_change_became_insufficient() {
		// Previous: full run with coverage met
		let run_full = create_run(60, Some(create_oracle(60, 0)), Some(create_derived(60, 0)), Some(create_backup(1000, 1000, 2000)));
		let prev = create_report(&run_full);

		// Current: tiny run with insufficient coverage
		let run_small = create_run(1, None, None, None);
		let curr = create_report(&run_small);

		let comp = compare_soak_reports(&prev, &curr, DEFAULT_METRIC_TOLERANCES);
		assert_eq!(comp.coverage_change, CoverageChange::BecameInsufficient, "coverage must have become insufficient");
	}
}
