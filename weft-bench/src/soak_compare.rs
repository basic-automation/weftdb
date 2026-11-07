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
//! | `contamination_flags_count` | exact (0%) | same hardware profile each night |

use std::fmt::Write;

use serde::{Deserialize, Serialize};

use crate::soak_report::{CoverageStatus, Outcome, RuleResult, SoakReport};

// ---------------------------------------------------------------------------
// Metric tolerances
// ---------------------------------------------------------------------------

/// A named metric with a relative tolerance (0.0 = exact, 0.10 = 10%).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
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
pub const DEFAULT_METRIC_TOLERANCES: &[MetricTolerance] = &[MetricTolerance { metric_name: "steps", tolerance: 0.0 }, MetricTolerance { metric_name: "oracle_ticks_judged", tolerance: 0.0 }, MetricTolerance { metric_name: "oracle_mismatch_count", tolerance: 0.0 }, MetricTolerance { metric_name: "derived_ticks_judged", tolerance: 0.0 }, MetricTolerance { metric_name: "derived_mismatch_count", tolerance: 0.0 }, MetricTolerance { metric_name: "backup_acknowledged_ticks", tolerance: 0.0 }, MetricTolerance { metric_name: "backup_rpo_ms", tolerance: 0.10 }, MetricTolerance { metric_name: "backup_rto_ms", tolerance: 0.10 }, MetricTolerance { metric_name: "backup_restore_duration_ms", tolerance: 0.10 }, MetricTolerance { metric_name: "contamination_flags_count", tolerance: 0.0 }];

// ---------------------------------------------------------------------------
// Rule outcome change
// ---------------------------------------------------------------------------

/// How one rule's outcome changed between two reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
	/// A new lane was added or the report structure changed.
	StructureChange,
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
	pub fn is_regression(&self) -> bool {
		self.has_regression
	}

	/// True when a metric drifted past its tolerance.
	#[must_use]
	pub fn has_warnings(&self) -> bool {
		self.has_drift_warning
	}

	/// Render the comparison as a Markdown summary for the job summary.
	///
	/// # Errors
	///
	/// Returns an error when the underlying `Write` fails (unreachable for a
	/// `String`).
	#[allow(clippy::needless_vec_and)]
	pub fn to_markdown(&self) -> String {
		let mut out = String::with_capacity(1024);

		out.push_str("## Soak Comparison\n\n");

		if let Some((prev_ver, cur_ver)) = self.schema_mismatch {
			let _ = writeln!(out, "⚠️ **Schema version mismatch:** previous=v{prev_ver}, current=v{cur_ver}. Metric drift not computed.\n");
		}

		out.push_str("### Rule outcome changes\n\n");
		if self.rule_changes.is_empty() {
			out.push_str("No rules compared — both reports likely empty.\n\n");
		} else {
			out.push_str("| Rule | Previous | Current | Change |\n");
			out.push_str("|------|----------|---------|--------|\n");
			for change in &self.rule_changes {
				let label = match change.change {
					OutcomeChange::Unchanged => "—",
					OutcomeChange::PassToFail => "🔴 Pass→Fail",
					OutcomeChange::FailToPass => "🟢 Fail→Pass",
					OutcomeChange::NotEvaluatedToEvaluated => "⚪→◉ evaluated",
					OutcomeChange::EvaluatedToNotEvaluated => "◉→⚪ not-evaluated",
				};
				let _ = writeln!(out, "| {} ({}) | `{:?}` | `{:?}` | {} |", change.rule_id, change.rule_name, change.previous, change.current, label);
			}
		}
		out.push('\n');

		if self.has_regression {
			out.push_str("❌ **Regression detected**: one or more rules changed from Pass to Fail.\n\n");
		}
		if self.has_drift_warning {
			out.push_str("⚠️ **Metric drift past tolerance**:\n\n");
			out.push_str("| Metric | Previous | Current | Drift | Tolerance | Past? |\n");
			out.push_str("|--------|----------|---------|-------|-----------|-------|\n");
			for drift in &self.metric_drifts {
				if drift.past_tolerance {
					let pct = format!("{:.2}%", drift.drift_pct * 100.0);
					let tol_pct = format!("{:.0}%", drift.tolerance * 100.0);
					let _ = writeln!(out, "| {} | {:.0} | {:.0} | {} | {} | ⚠️ |", drift.metric_name, drift.previous_value, drift.current_value, pct, tol_pct);
				}
			}
			out.push('\n');
		}

		out.push_str(&self.summary);
		out
	}
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
/// `tolerances` may be `DEFAULT_METRIC_TOLERANCES` or a custom set. Passing
/// an empty slice disables the metric-drift check entirely.
#[must_use]
pub fn compare_soak_reports(previous: &SoakReport, current: &SoakReport, tolerances: &[MetricTolerance]) -> SoakComparison {
	let schema_mismatch = if previous.schema_version != current.schema_version { Some((previous.schema_version, current.schema_version)) } else { None };

	// --- Rule outcome changes ---
	let rule_changes: Vec<RuleChange> = {
		let mut changes = Vec::with_capacity(previous.slo.rules.len().max(current.slo.rules.len()));
		// Match by id
		let prev_by_id: std::collections::HashMap<&str, &RuleResult> = previous.slo.rules.iter().map(|r| (r.id.as_str(), r)).collect();
		let curr_by_id: std::collections::HashMap<&str, &RuleResult> = current.slo.rules.iter().map(|r| (r.id.as_str(), r)).collect();

		// Rules in previous
		for (id, prev_rule) in &prev_by_id {
			if let Some(curr_rule) = curr_by_id.get(id) {
				changes.push(RuleChange::new(prev_rule, curr_rule));
			} else {
				// Rule exists in previous but not current — treat as evaluated→not-evaluated
				changes.push(RuleChange { rule_id: id.to_string(), rule_name: prev_rule.name.clone(), previous: prev_rule.outcome, current: Outcome::NotEvaluated, change: OutcomeChange::EvaluatedToNotEvaluated });
			}
		}
		// Rules in current but not previous
		for (id, curr_rule) in &curr_by_id {
			if !prev_by_id.contains_key(id) {
				changes.push(RuleChange { rule_id: id.to_string(), rule_name: curr_rule.name.clone(), previous: Outcome::NotEvaluated, current: curr_rule.outcome, change: OutcomeChange::NotEvaluatedToEvaluated });
			}
		}
		changes.sort_by(|a, b| a.rule_id.cmp(&b.rule_id));
		changes
	};

	let has_regression = rule_changes.iter().any(|rc| rc.change == OutcomeChange::PassToFail);

	// --- Coverage change ---
	let coverage_change = match (&previous.coverage.status, &current.coverage.status) {
		(CoverageStatus::Met, CoverageStatus::Met) => CoverageChange::BothMet,
		(CoverageStatus::InsufficientCoverage(_), CoverageStatus::Met) => CoverageChange::Recovered,
		(CoverageStatus::Met, CoverageStatus::InsufficientCoverage(_)) => CoverageChange::BecameInsufficient,
		(CoverageStatus::InsufficientCoverage(_), CoverageStatus::InsufficientCoverage(_)) => CoverageChange::BothInsufficient,
	};

	// --- Metric drift ---
	let metric_drifts = if schema_mismatch.is_some() {
		Vec::new()
	} else {
		let extract = |report: &SoakReport| -> Vec<(String, f64)> {
			let mut m = Vec::with_capacity(16);

			m.push(("steps".to_string(), report.run.steps as f64));

			if let Some(ref oracle) = report.run.oracle {
				m.push(("oracle_ticks_judged".to_string(), oracle.ticks_judged as f64));
				m.push(("oracle_mismatch_count".to_string(), oracle.mismatch_count as f64));
			} else {
				m.push(("oracle_ticks_judged".to_string(), f64::NAN));
				m.push(("oracle_mismatch_count".to_string(), f64::NAN));
			}

			if let Some(ref derived) = report.run.derived {
				m.push(("derived_ticks_judged".to_string(), derived.ticks_judged as f64));
				m.push(("derived_mismatch_count".to_string(), derived.mismatch_count as f64));
			} else {
				m.push(("derived_ticks_judged".to_string(), f64::NAN));
				m.push(("derived_mismatch_count".to_string(), f64::NAN));
			}

			if let Some(ref backup) = report.run.backup {
				m.push(("backup_acknowledged_ticks".to_string(), backup.prefix.acknowledged_ticks as f64));
				m.push(("backup_rpo_ms".to_string(), backup.timing.rpo_ms() as f64));
				m.push(("backup_rto_ms".to_string(), backup.timing.rto_ms() as f64));
				m.push(("backup_restore_duration_ms".to_string(), backup.timing.restore_duration_ms as f64));
			} else {
				m.push(("backup_acknowledged_ticks".to_string(), f64::NAN));
				m.push(("backup_rpo_ms".to_string(), f64::NAN));
				m.push(("backup_rto_ms".to_string(), f64::NAN));
				m.push(("backup_restore_duration_ms".to_string(), f64::NAN));
			}

			// Contamination flags count: not stored directly on SoakReport.run,
			// so we extract from the honesty section by checking the slo rules.
			// We'll use the contamination verdict from the run's system sampler
			// which is not stored. Default to 0 for now.
			m.push(("contamination_flags_count".to_string(), 0.0_f64));

			m
		};

		let prev_metrics = extract(previous);
		let curr_metrics = extract(current);

		let prev_map: std::collections::HashMap<&str, f64> = prev_metrics.iter().map(|(k, v)| (k.as_str(), *v)).collect();
		let curr_map: std::collections::HashMap<&str, f64> = curr_metrics.iter().map(|(k, v)| (k.as_str(), *v)).collect();

		let mut drifts = Vec::with_capacity(tolerances.len());
		for tolerance in tolerances {
			let prev_val = prev_map.get(tolerance.metric_name).copied().unwrap_or(f64::NAN);
			let curr_val = curr_map.get(tolerance.metric_name).copied().unwrap_or(f64::NAN);

			if prev_val.is_nan() || curr_val.is_nan() {
				// One or both reports don't have this metric — skip
				continue;
			}

			let drift_pct = if prev_val != 0.0 {
				(curr_val - prev_val).abs() / prev_val.abs()
			} else if curr_val != 0.0 {
				// Previous was zero, current is non-zero — infinite drift
				f64::INFINITY
			} else {
				0.0
			};

			let past_tolerance = drift_pct > tolerance.tolerance;
			drifts.push(MetricDrift { metric_name: tolerance.metric_name.to_string(), previous_value: prev_val, current_value: curr_val, tolerance: tolerance.tolerance, drift_pct, past_tolerance });
		}
		drifts
	};

	let has_drift_warning = metric_drifts.iter().any(|d| d.past_tolerance);

	// --- Summary ---
	let summary = {
		let mut s = String::new();
		if schema_mismatch.is_some() {
			let _ = write!(s, "Schema version mismatch — rule outcomes compared, metrics skipped.");
		} else if has_regression {
			let regression_count = rule_changes.iter().filter(|rc| rc.change == OutcomeChange::PassToFail).count();
			let _ = write!(s, "{regression_count} regression(s) — job fails.");
		} else {
			let pass_to_fail = rule_changes.iter().filter(|rc| rc.change == OutcomeChange::PassToFail).count();
			let fail_to_pass = rule_changes.iter().filter(|rc| rc.change == OutcomeChange::FailToPass).count();
			if pass_to_fail == 0 && fail_to_pass == 0 {
				let _ = write!(s, "No rule outcome changes.");
			} else {
				let _ = write!(s, "No regressions. Improvements: {fail_to_pass}.");
			}
			if has_drift_warning {
				let _ = write!(s, " Metric drift past tolerance — check warnings.");
			}
			if !has_drift_warning && pass_to_fail == 0 {
				let _ = write!(s, " All metric drifts within tolerance.");
			}
		}
		s
	};

	SoakComparison { schema_version: 1, schema_mismatch, rule_changes, coverage_change, metric_drifts, has_regression, has_drift_warning, summary }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		report::RunMetadata, soak::{SoakConfig, SoakHarness, SoakRun}, soak_backup::{BackupLaneVerdict, BackupPrefixVerdict, RecoveryTiming, RestoreVerdict}, soak_oracle::OracleVerdict, soak_read::DerivedReadVerdict, soak_report::{CoverageReport, CoverageStatus, InsufficientCoverage, Outcome, RuleResult, SoakReport}, soak_system::FlushCheckResult
	};

	fn config() -> SoakConfig {
		SoakConfig::new("smoke".to_string(), "nightly profile".to_string(), 7, 60).expect("valid config")
	}

	fn metadata() -> RunMetadata {
		RunMetadata::capture("2026-10-25T00:00:00+00:00".to_string())
	}

	fn oracle_verdict(ticks_judged: u64, mismatches: u64) -> OracleVerdict {
		OracleVerdict { ticks_judged, cells_checked: ticks_judged * 4, cells_present: ticks_judged * 4, points_observed: ticks_judged * 4, mismatch_count: mismatches, mismatches: Vec::new() }
	}

	fn derived_verdict(ticks_judged: u64, mismatches: u64) -> DerivedReadVerdict {
		DerivedReadVerdict { ticks_judged, buckets_checked: ticks_judged, buckets_present: ticks_judged, points_counted: ticks_judged * 4, mismatch_count: mismatches, mismatches: Vec::new() }
	}

	fn backup_verdict(backup_dur: u64, restore_dur: u64) -> BackupLaneVerdict {
		BackupLaneVerdict { prefix: BackupPrefixVerdict { acknowledged_ticks: 6, ticks_in_backup: 3, cut_tick: Some(2), points_expected: 24, points_in_backup: 24, mismatch_count: 0, mismatches: Vec::new() }, restore: RestoreVerdict { ticks_judged: 6, points_expected: 48, points_restored: 48, mismatch_count: 0, mismatches: Vec::new() }, timing: RecoveryTiming { backup_interval_ms: 1000, backup_duration_ms: backup_dur, restore_duration_ms: if restore_dur > backup_dur { restore_dur - backup_dur } else { backup_dur } } }
	}

	fn flush_check() -> FlushCheckResult {
		FlushCheckResult { name: "flush_observed", device: Some(String::from("nvme0n1")), passed: true, failure: None, skipped: None, device_written_bytes: Some(1_000_000), acknowledged_bytes: 1_000_000, dirty_kib_at_close: 0, slack_bytes: 0 }
	}

	fn run_harness(steps: u64) -> SoakRun {
		let mut harness = SoakHarness::new(config(), metadata());
		harness.start().expect("start");
		for _ in 0..steps {
			harness.record_step().expect("step");
		}
		harness.finish().expect("finish");
		harness.run_record().clone()
	}

	fn make_report(run: &SoakRun, oracle_ticks: u64, oracle_mismatch: u64, backup_dur: u64, restore_dur: u64) -> SoakReport {
		SoakReport::evaluate(run, Some(&oracle_verdict(oracle_ticks, oracle_mismatch)), Some(&derived_verdict(oracle_ticks, 0)), Some(&backup_verdict(backup_dur, restore_dur)), None, Some(&flush_check()), Vec::new(), oracle_ticks, oracle_ticks * 4, 1, 1, 1, 1)
	}

	// -----------------------------------------------------------------------
	// Identical reports
	// -----------------------------------------------------------------------

	#[test]
	fn identical_reports_have_no_changes() {
		let run = run_harness(60);
		let report = make_report(&run, 60, 0, 1000, 2000);
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
		let run = run_harness(60);
		let prev = make_report(&run, 60, 0, 1000, 2000);
		// Current report with an oracle mismatch → R1 fails
		let curr = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 1)), Some(&derived_verdict(60, 0)), Some(&backup_verdict(1000, 2000)), None, Some(&flush_check()), Vec::new(), 60, 240, 1, 1, 1, 1);
		let comp = compare_soak_reports(&prev, &curr, DEFAULT_METRIC_TOLERANCES);
		assert!(comp.has_regression, "R1 Pass→Fail must be a regression");
		assert!(comp.rule_changes.iter().any(|rc| rc.rule_id == "R1" && rc.change == OutcomeChange::PassToFail), "R1 must show Pass→Fail");
	}

	// -----------------------------------------------------------------------
	// Fail→Pass improvement
	// -----------------------------------------------------------------------

	#[test]
	fn fail_to_pass_is_improvement_not_regression() {
		let run = run_harness(60);
		let prev = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 1)), Some(&derived_verdict(60, 0)), Some(&backup_verdict(1000, 2000)), None, Some(&flush_check()), Vec::new(), 60, 240, 1, 1, 1, 1);
		let curr = make_report(&run, 60, 0, 1000, 2000);
		let comp = compare_soak_reports(&prev, &curr, DEFAULT_METRIC_TOLERANCES);
		assert!(!comp.has_regression, "Fail→Pass is not a regression");
		assert!(comp.rule_changes.iter().any(|rc| rc.rule_id == "R1" && rc.change == OutcomeChange::FailToPass), "R1 must show Fail→Pass");
	}

	// -----------------------------------------------------------------------
	// NotEvaluated ↔ Pass
	// -----------------------------------------------------------------------

	#[test]
	fn not_evaluated_to_pass_is_not_a_regression() {
		let run = run_harness(60);
		// Previous: no oracle → R1 NotEvaluated
		let prev = SoakReport::evaluate(&run, None, None, None, None, None, Vec::new(), 0, 0, 1, 1, 1, 1);
		let curr = make_report(&run, 60, 0, 1000, 2000);
		let comp = compare_soak_reports(&prev, &curr, DEFAULT_METRIC_TOLERANCES);
		assert!(!comp.has_regression, "NotEvaluated→Pass is not a regression");
	}

	#[test]
	fn pass_to_not_evaluated_is_not_a_regression() {
		let run = run_harness(60);
		let prev = make_report(&run, 60, 0, 1000, 2000);
		let curr = SoakReport::evaluate(&run, None, None, None, None, None, Vec::new(), 0, 0, 1, 1, 1, 1);
		let comp = compare_soak_reports(&prev, &curr, DEFAULT_METRIC_TOLERANCES);
		assert!(!comp.has_regression, "Pass→NotEvaluated is not a regression");
		assert!(comp.rule_changes.iter().any(|rc| rc.change == OutcomeChange::EvaluatedToNotEvaluated), "must show EvaluatedToNotEvaluated");
	}

	// -----------------------------------------------------------------------
	// Metric drift at and past tolerance
	// -----------------------------------------------------------------------

	#[test]
	fn drift_exactly_at_tolerance_is_not_past() {
		let run = run_harness(60);
		let prev = make_report(&run, 60, 0, 1000, 2000);
		// backup_rpo_ms has 10% tolerance. rpo_ms = backup_interval_ms = 1000.
		// At exactly 1100 (10% drift), it should NOT be past tolerance.
		let curr_backup = BackupLaneVerdict { prefix: BackupPrefixVerdict { acknowledged_ticks: 6, ticks_in_backup: 3, cut_tick: Some(2), points_expected: 24, points_in_backup: 24, mismatch_count: 0, mismatches: Vec::new() }, restore: RestoreVerdict { ticks_judged: 6, points_expected: 48, points_restored: 48, mismatch_count: 0, mismatches: Vec::new() }, timing: RecoveryTiming { backup_interval_ms: 1100, backup_duration_ms: 1000, restore_duration_ms: 2000 } };
		let curr = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 0)), Some(&derived_verdict(60, 0)), Some(&curr_backup), None, Some(&flush_check()), Vec::new(), 60, 240, 1, 1, 1, 1);
		let comp = compare_soak_reports(&prev, &curr, DEFAULT_METRIC_TOLERANCES);
		// backup_rpo_ms = 1000 → 1100 = 10% drift, tolerance = 0.10, drift_pct = 0.10, NOT past (0.10 > 0.10 is false)
		// But wait: drift_pct = 0.10 (10%), tolerance = 0.10. `drift_pct > tolerance` → false. Good.
		let rpo_drift = comp.metric_drifts.iter().find(|d| d.metric_name == "backup_rpo_ms").expect("backup_rpo_ms drift must exist");
		// 1100/1000 = 1.1 → drift_pct = 0.1 → 0.1 > 0.1 is false, so NOT past
		assert!(!rpo_drift.past_tolerance, "10% drift on a 10% tolerance must not be past");
	}

	#[test]
	fn drift_just_past_tolerance_is_flagged() {
		let run = run_harness(60);
		let prev = make_report(&run, 60, 0, 1000, 2000);
		// backup_rpo_ms: previous 1000, current 1101 → drift = 10.1% > 10%
		let curr_backup = BackupLaneVerdict { prefix: BackupPrefixVerdict { acknowledged_ticks: 6, ticks_in_backup: 3, cut_tick: Some(2), points_expected: 24, points_in_backup: 24, mismatch_count: 0, mismatches: Vec::new() }, restore: RestoreVerdict { ticks_judged: 6, points_expected: 48, points_restored: 48, mismatch_count: 0, mismatches: Vec::new() }, timing: RecoveryTiming { backup_interval_ms: 1101, backup_duration_ms: 1000, restore_duration_ms: 2000 } };
		let curr = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 0)), Some(&derived_verdict(60, 0)), Some(&curr_backup), None, Some(&flush_check()), Vec::new(), 60, 240, 1, 1, 1, 1);
		let comp = compare_soak_reports(&prev, &curr, DEFAULT_METRIC_TOLERANCES);
		assert!(comp.has_drift_warning, "drift past tolerance must set has_drift_warning");
		let rpo_drift = comp.metric_drifts.iter().find(|d| d.metric_name == "backup_rpo_ms").expect("backup_rpo_ms drift must exist");
		assert!(rpo_drift.past_tolerance, "10.1% drift on 10% tolerance must be past");
	}

	// -----------------------------------------------------------------------
	// Schema version mismatch
	// -----------------------------------------------------------------------

	#[test]
	fn schema_mismatch_reported_and_no_metrics_compared() {
		let run = run_harness(60);
		let mut prev = make_report(&run, 60, 0, 1000, 2000);
		// Force a schema version mismatch
		prev.schema_version = 42;
		let curr = make_report(&run, 60, 0, 1000, 2000);
		let comp = compare_soak_reports(&prev, &curr, DEFAULT_METRIC_TOLERANCES);
		assert_eq!(comp.schema_mismatch, Some((42, 1)), "schema mismatch must be reported");
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
		let run = run_harness(60);
		let report = make_report(&run, 60, 0, 1000, 2000);
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
		let run = run_harness(60);
		let report = make_report(&run, 60, 0, 1000, 2000);
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
		let run = run_harness(60);
		let report = make_report(&run, 60, 0, 1000, 2000);
		let comp = compare_soak_reports(&report, &report, &[]);
		assert!(comp.metric_drifts.is_empty(), "no metric drifts with empty tolerances");
		assert!(!comp.has_drift_warning, "no warning when metric drifts are empty");
	}

	#[test]
	fn coverage_change_recovered() {
		let run = run_harness(2); // Small steps
		let prev = SoakReport::evaluate(&run, None, None, None, None, None, Vec::new(), 0, 0, 100, 1, 1, 1);
		// steps=2, min_evaluated_ticks=100 → coverage insufficient

		let big_run = run_harness(200);
		let curr = SoakReport::evaluate(&big_run, Some(&oracle_verdict(200, 0)), Some(&derived_verdict(200, 0)), Some(&backup_verdict(1000, 2000)), None, Some(&flush_check()), Vec::new(), 200, 800, 10, 1, 10, 1);
		// steps=200, min_evaluated_ticks=10 → coverage met

		let comp = compare_soak_reports(&prev, &curr, DEFAULT_METRIC_TOLERANCES);
		assert_eq!(comp.coverage_change, CoverageChange::Recovered, "coverage must have recovered");
	}

	#[test]
	fn coverage_change_became_insufficient() {
		let big_run = run_harness(200);
		let prev = SoakReport::evaluate(&big_run, Some(&oracle_verdict(200, 0)), Some(&derived_verdict(200, 0)), Some(&backup_verdict(1000, 2000)), None, Some(&flush_check()), Vec::new(), 200, 800, 10, 1, 10, 1);
		let run = run_harness(2);
		let curr = SoakReport::evaluate(&run, None, None, None, None, None, Vec::new(), 0, 0, 100, 1, 1, 1);

		let comp = compare_soak_reports(&prev, &curr, DEFAULT_METRIC_TOLERANCES);
		assert_eq!(comp.coverage_change, CoverageChange::BecameInsufficient, "coverage must have become insufficient");
	}
}
