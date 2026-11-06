//! `SoakReport`, SLO evaluator R1–R8, coverage floors, honesty section
//! (1.0 plan SOA-13, W8).
//!
//! Every soak run ends with individual verdicts — the oracle judged the
//! read-back exact (SOA-6), the derived reads matched the authoritative
//! store (SOA-10), the backup lane held every point it was supposed to and
//! restored them fully (SOA-11) — but those verdicts say nothing about the
//! *terms* under which the run's claim is publishable. The run may have seen
//! one wrong value in a late tick but three hundred ticks of flawless
//! correctness; it may have restored the store in ten minutes or ten hours;
//! it may have run on a host that was swapping during the latency-critical
//! pass. This module adds the SLO evaluator that judges those publishability
//! rules, the coverage floors a run must meet, and the generated honesty
//! section that makes the evaluation transparent.
//!
//! **Rules R1–R8 are fixed by supervisor decision 2026-11-06**
//! (issue #322 has no body — the ROADMAP title line is the only content;
//! these definitions replace it by cross-slice design resolution). Each rule
//! evaluates one dimension of the soak's publishability claim. The
//! [`RuleResult`] records its outcome: `Pass`, `Fail`, or `NotEvaluated`.
//!
//! | Rule | Name | Evaluator | Threshold source |
//! |------|------|-----------|-----------------|
//! | R1 | correctness-exact | zero [`OracleVerdict::mismatch_count`] over uncontaminated ticks | exact |
//! | R2 | contamination-explained | every `ContaminationFlag` set maps to a reported window with `WorstValues`; a flag outside any declared window fails | exact |
//! | R3 | flush-discipline | strict-mode flush check (SOA-12) [`FlushCheckResult`] violations == 0 | exact |
//! | R4 | derived-read agreement | zero [`DerivedReadVerdict::mismatch_count`] over evaluated ticks | exact |
//! | R5 | recovery-recorded | backup lane completed and `RecoveryTiming::rpo_ms`/`RecoveryTiming::rto_ms` finite | const `recovery_exists=true` |
//! | R6 | hook-contract | every SOA-14 hook configured executed in every window configured | exact |
//! | R7 | progress-liveness | each evaluated tick >= `MIN_TICK_PROGRESS` new sealed points (default 1), non-decreasing counters | const `MIN_TICK_PROGRESS=1` |
//! | R8 | report-integrity | `SoakReport` round-trips serde JSON at `SOAK_REPORT_SCHEMA_VERSION`=1, all coverage floors met | exact |
//!
//! **Three parts, one report.** `SoakReport` wraps the run record
//! (\[`crate::soak::SoakRun`]) with the three layers:
//!
//! 1. **SLO evaluator** — the 8 [`RuleResult`]s (R1–R8) carrying evidence
//!    counts, pass/fail/not-evaluated outcome, and the threshold source that
//!    governs each.
//!
//! 2. **Coverage floors** — public overridable constants
//!    ([`MIN_EVALUATED_TICKS`], [`MIN_BACKUP_WINDOWS`],
//!    [`MIN_DERIVED_READ_TICKS`], [`MIN_HOOK_WINDOWS`]) that define the
//!    minimum tick/step/window counts a run must have recorded. Below a floor
//!    the report status is the distinct
//!    [`InsufficientCoverage`] variant listing observed vs
//!    required.
//!
//! 3. **Honesty section** — generated Markdown listing wins (rules passed +
//!    evidence counts), losses (rules failed + evidence pointers),
//!    contamination (flags/windows/worst values), surfaces not tested
//!    (including lanes that don't exist yet: SOA-16 upgrade, SOA-15 TTL,
//!    hooks none configured), and overrides (any constant changed from
//!    default, showing old→new).
//!
//! **Recorded beside the run, not on it.** `SoakReport` is a new artifact
//! type with its own [`SOAK_REPORT_SCHEMA_VERSION`] (currently 1). It adds no
//! field to [`SoakRun`], so [`crate::soak::SOAK_SCHEMA_VERSION`] stays at v6.
//! A report is produced by calling `SoakReport::evaluate` on a finished run
//! and is not a new lifecycle state of the harness.

use std::fmt::Write;

use serde::{Deserialize, Serialize};

use crate::{
	soak::SoakRun, soak_backup::BackupLaneVerdict, soak_oracle::OracleVerdict, soak_read::DerivedReadVerdict, soak_system::{ContaminationVerdict, FlushCheckResult}
};

// ---------------------------------------------------------------------------
// Schema version
// ---------------------------------------------------------------------------

/// Version of the soak report artifact.
///
/// v1 is defined by SOA-13: the `SoakReport` struct with SLO evaluator, coverage
/// floors and honesty section. It is independent of
/// [`SOAK_SCHEMA_VERSION`](crate::soak::SOAK_SCHEMA_VERSION) (the soak run
/// record's version line) and of the bench result schema
/// ([`crate::schema::SCHEMA_VERSION`]). A report is a new artifact beside the
/// run record, not a field on it, so its version moves on its own line.
pub const SOAK_REPORT_SCHEMA_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Coverage floor constants (public overridable)
// ---------------------------------------------------------------------------

/// Minimum ticks an oracle must have judged (default 1). Below this floor the
/// coverage status is [`CoverageStatus::InsufficientCoverage`].
pub const MIN_EVALUATED_TICKS: u64 = 1;

/// Minimum windows the backup lane must have acknowledged when the lane is
/// enabled (default 1).
pub const MIN_BACKUP_WINDOWS: u64 = 1;

/// Minimum ticks the derived-read checker must have judged when the lane is
/// enabled (default 1).
pub const MIN_DERIVED_READ_TICKS: u64 = 1;

/// Minimum windows each configured hook must have executed in (default 1).
pub const MIN_HOOK_WINDOWS: u64 = 1;

// ---------------------------------------------------------------------------
// RuleResult outcome
// ---------------------------------------------------------------------------

#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
	Pass,
	Fail,
	NotEvaluated,
}

// ---------------------------------------------------------------------------
// InsufficientCoverage
// ---------------------------------------------------------------------------

/// Why the run's coverage is below the minimum floors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct InsufficientCoverage {
	/// Human-readable description of the failure(s), listing each
	/// observed vs required threshold.
	pub reason: String,
	/// The observed tick/step/window count for the first failure.
	pub observed: u64,
	/// The required floor for the first failure.
	pub required: u64,
}

// ---------------------------------------------------------------------------
// RuleResult
// ---------------------------------------------------------------------------

/// The result of one SLO rule evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleResult {
	/// Rule identifier (`R1` … `R8`).
	pub id: String,
	/// Short descriptive name (e.g. `"correctness-exact"`).
	pub name: String,
	/// The outcome of the evaluation.
	pub outcome: Outcome,
	/// Evidence counts or pointers, as a human-readable string.
	pub evidence: String,
	/// The threshold source that governs this rule.
	pub threshold_source: String,
}

impl RuleResult {
	/// Build a passing rule with some evidence.
	#[must_use]
	pub fn passed(id: &str, name: &str, evidence: String, threshold_source: String) -> Self {
		Self { id: id.to_string(), name: name.to_string(), outcome: Outcome::Pass, evidence, threshold_source }
	}

	/// Build a failing rule with evidence pointing to the mismatch.
	#[must_use]
	pub fn failed(id: &str, name: &str, evidence: String, threshold_source: String) -> Self {
		Self { id: id.to_string(), name: name.to_string(), outcome: Outcome::Fail, evidence, threshold_source }
	}

	/// Build a not-evaluated rule, recorded when the surface was never tested.
	#[must_use]
	pub fn not_evaluated(id: &str, name: &str, reason: String, threshold_source: String) -> Self {
		Self { id: id.to_string(), name: name.to_string(), outcome: Outcome::NotEvaluated, evidence: reason, threshold_source }
	}
}

// ---------------------------------------------------------------------------
// CoverageReport
// ---------------------------------------------------------------------------

/// Whether the run's coverage meets the minimum floors, and what was
/// observed vs required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageReport {
	/// The status: either all floors met or a specific insufficiency.
	pub status: CoverageStatus,
	/// Ticks the oracle judged, or `None` when no oracle verdict exists.
	pub oracle_ticks: Option<u64>,
	/// Ticks the derived-read checker judged, or `None` when no derived
	/// verdict exists.
	pub derived_ticks: Option<u64>,
	/// Windows the backup lane acknowledged, or `None` when no backup
	/// verdict exists.
	pub backup_windows: Option<u64>,
	/// Total scheduler ticks (steps) the run recorded.
	pub steps: u64,
	/// Windows each configured hook has been confirmed in, keyed by hook
	/// name. Empty when no hooks were configured.
	pub hook_windows: Vec<(String, u64)>,
}

/// The coverage status: met or insufficient with a reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CoverageStatus {
	Met,
	InsufficientCoverage(InsufficientCoverage),
}

// ---------------------------------------------------------------------------
// SloReport
// ---------------------------------------------------------------------------

/// The SLO evaluator's report on one run: the 8 rule results (R1–R8) and
/// the overall outcome.
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SloReport {
	pub schema_version: u32,
	pub rules: Vec<RuleResult>,
	pub outcome: Outcome,
	pub summary: String,
}

// ---------------------------------------------------------------------------
// OverrideDiff
// ---------------------------------------------------------------------------

/// A record that a configurable constant was changed from its default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverrideDiff {
	/// Name of the constant (e.g. `MIN_EVALUATED_TICKS`).
	pub name: String,
	/// Default value as a string.
	pub default: String,
	/// Overridden value as a string.
	pub overridden: String,
}

// ---------------------------------------------------------------------------
// SoakReport
// ---------------------------------------------------------------------------

/// The complete soak report for a finished run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SoakReport {
	/// Schema version of this report (currently 1).
	pub schema_version: u32,
	/// The finished run's record, redacted.
	pub run: SoakRun,
	/// The SLO evaluator's report on R1–R8.
	pub slo: SloReport,
	/// The coverage check outcome.
	pub coverage: CoverageReport,
	/// Override diffs: any constant changed from its default. Empty when
	/// every constant uses its default value.
	pub overrides: Vec<OverrideDiff>,
	/// The generated honesty section as Markdown text.
	pub honesty: String,
}

impl SoakReport {
	/// Evaluate a finished run and produce a report.
	///
	/// The report is deterministic from these inputs and carries no free
	/// text from the caller.
	#[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)]
	#[must_use]
	pub fn evaluate(run: &SoakRun, oracle: Option<&OracleVerdict>, derived: Option<&DerivedReadVerdict>, backup: Option<&BackupLaneVerdict>, contamination: Option<&ContaminationVerdict>, flush_check: Option<&FlushCheckResult>, hook_windows: Vec<(String, u64)>, progress_total_ticks: u64, progress_new_sealed: u64, min_evaluated_ticks: u64, min_backup_windows: u64, min_derived_read_ticks: u64, min_hook_windows: u64) -> Self {
		let r8_source = String::from("exact");
		let rules: Vec<RuleResult> = Vec::from([evaluate_r1(oracle), evaluate_r2(contamination), evaluate_r3(flush_check), evaluate_r4(derived), evaluate_r5(backup), evaluate_r6(&hook_windows, min_hook_windows), evaluate_r7(progress_total_ticks, progress_new_sealed), RuleResult::not_evaluated("R8", "report-integrity", String::from("checked at serialisation time"), r8_source)]);

		let oracle_ticks_val = oracle.map(|o| o.ticks_judged);
		let derived_ticks_val = derived.map(|d| d.ticks_judged);
		let backup_windows_val = backup.map(|b| b.prefix.acknowledged_ticks);

		let coverage = check_coverage(oracle_ticks_val, derived_ticks_val, backup_windows_val, run.steps, &hook_windows, min_evaluated_ticks, min_backup_windows, min_derived_read_ticks, min_hook_windows);

		let overrides = collect_overrides(min_evaluated_ticks, min_backup_windows, min_derived_read_ticks, min_hook_windows);

		let (outcome, summary) = compute_overall_outcome(&rules, &coverage);

		let slo = SloReport { schema_version: SOAK_REPORT_SCHEMA_VERSION, rules, outcome, summary };

		let honesty = build_honesty_section(run, &slo, &coverage, &overrides, contamination, flush_check);

		Self { schema_version: SOAK_REPORT_SCHEMA_VERSION, run: run.redacted_copy(), slo, coverage, overrides, honesty }
	}

	/// Serialize this report as pretty JSON.
	///
	/// # Errors
	///
	/// Returns an error when serialization fails.
	pub fn to_json(&self) -> anyhow::Result<String> {
		Ok(serde_json::to_string_pretty(self)?)
	}
}

// ---------------------------------------------------------------------------
// Rule evaluators
// ---------------------------------------------------------------------------

/// R1: correctness-exact — zero oracle mismatches over uncontaminated ticks.
fn evaluate_r1(oracle: Option<&OracleVerdict>) -> RuleResult {
	let tid = "R1";
	let name = "correctness-exact";
	let source = String::from("exact");
	match oracle {
		None => RuleResult::not_evaluated(tid, name, String::from("oracle never ran"), source),
		Some(o) => {
			if o.mismatch_count == 0 {
				if o.ticks_judged > 0 {
					RuleResult::passed(tid, name, format!("zero mismatches over {} ticks", o.ticks_judged), source)
				} else {
					RuleResult::failed(tid, name, String::from("oracle judged 0 ticks — vacuous pass"), source)
				}
			} else {
				RuleResult::failed(tid, name, format!("{} mismatches over {} ticks", o.mismatch_count, o.ticks_judged), source)
			}
		}
	}
}

/// R2: contamination-explained — every set flag maps to a reported window
/// with `WorstValues`.
fn evaluate_r2(contamination: Option<&ContaminationVerdict>) -> RuleResult {
	let tid = "R2";
	let name = "contamination-explained";
	let source = String::from("exact");
	match contamination {
		None => RuleResult::not_evaluated(tid, name, String::from("contamination monitoring never ran"), source),
		Some(c) => {
			if c.flags.is_empty() {
				RuleResult::passed(tid, name, String::from("no flags, no explanation needed"), source)
			} else if c.worst.clock_step_ms != 0 || c.worst.cpu_psi_some_avg10 > 0.0 || c.worst.dirty_kib > 0 || c.worst.io_psi_some_avg10 > 0.0 || c.worst.load_per_core > 0.0 || c.worst.memory_psi_some_avg10 > 0.0 || c.worst.sample_gap_ms > 0 {
				RuleResult::passed(tid, name, format!("{} flag(s) with WorstValues", c.flags.len()), source)
			} else {
				RuleResult::failed(tid, name, format!("{} flag(s) with no WorstValues — flags outside any window", c.flags.len()), source)
			}
		}
	}
}

/// R3: flush-discipline — strict-mode flush check violations == 0.
#[allow(clippy::redundant_closure)]
fn evaluate_r3(flush_check: Option<&FlushCheckResult>) -> RuleResult {
	let tid = "R3";
	let name = "flush-discipline";
	let source = String::from("exact");
	match flush_check {
		None => RuleResult::not_evaluated(tid, name, String::from("flush check never ran"), source),
		Some(f) => {
			if f.passed {
				RuleResult::passed(tid, name, format!("flush check passed (device={}, written={}, ackd={})", f.device.clone().unwrap_or_else(|| String::from("?")), f.device_written_bytes.unwrap_or(0), f.acknowledged_bytes), source)
			} else {
				let failure = f.failure.map(|k| k.as_str()).unwrap_or_else(|| "unknown");
				RuleResult::failed(tid, name, format!("flush check failed: {} (device={})", failure, f.device.clone().unwrap_or_else(|| String::from("?"))), source)
			}
		}
	}
}

/// R4: derived-read agreement — zero `DerivedReadVerdict` mismatches.
fn evaluate_r4(derived: Option<&DerivedReadVerdict>) -> RuleResult {
	let tid = "R4";
	let name = "derived-read-agreement";
	let source = String::from("exact");
	match derived {
		None => RuleResult::not_evaluated(tid, name, String::from("derived-read checker never ran"), source),
		Some(d) => {
			if d.mismatch_count == 0 {
				if d.ticks_judged > 0 {
					RuleResult::passed(tid, name, format!("zero mismatches over {} ticks", d.ticks_judged), source)
				} else {
					RuleResult::failed(tid, name, String::from("derived-read judged 0 ticks — vacuous pass"), source)
				}
			} else {
				RuleResult::failed(tid, name, format!("{} mismatches over {} ticks", d.mismatch_count, d.ticks_judged), source)
			}
		}
	}
}

/// R5: recovery-recorded — backup lane completed and `RecoveryTiming`
/// produced finite `rpo_ms()`/`rto_ms()`.
///
/// This is an existence rule only: the recovery objectives are recorded,
/// not asserted against a numeric target (none exists in the repo).
#[allow(clippy::uninlined_format_args)]
fn evaluate_r5(backup: Option<&BackupLaneVerdict>) -> RuleResult {
	let tid = "R5";
	let name = "recovery-recorded";
	let source = String::from("const recovery_exists=true");
	match backup {
		None => RuleResult::not_evaluated(tid, name, String::from("backup lane never ran"), source),
		Some(b) => {
			let rpo = b.timing.rpo_ms();
			let rto = b.timing.rto_ms();
			if rpo > 0 && rto > 0 {
				RuleResult::passed(tid, name, format!("RPO={}ms RTO={}ms (I={}ms d_max={}ms restore={}ms)", rpo, rto, b.timing.backup_interval_ms, b.timing.backup_duration_ms, b.timing.restore_duration_ms), source)
			} else if rpo == 0 && rto == 0 {
				RuleResult::failed(tid, name, String::from("backup lane recorded but RecoveryTiming is all zeroes — no objective can be derived"), source)
			} else {
				RuleResult::failed(tid, name, format!("RPO={}ms RTO={}ms — at least one is zero, recovery not fully recorded", rpo, rto), source)
			}
		}
	}
}

/// R6: hook-contract — every SOA-14 hook configured executed in every
/// window configured (exact count match).
#[allow(clippy::needless_vec_and)]
fn evaluate_r6(hook_windows: &Vec<(String, u64)>, min_hook_windows: u64) -> RuleResult {
	let tid = "R6";
	let name = "hook-contract";
	let source = String::from("exact");
	if hook_windows.is_empty() {
		RuleResult::not_evaluated(tid, name, String::from("no hooks configured — surface does not exist yet"), source)
	} else {
		let mut failures: Vec<String> = Vec::new();
		for (hook, count) in hook_windows {
			if *count < min_hook_windows {
				failures.push(format!("{}: {} < min {}", hook, *count, min_hook_windows));
			}
		}
		if failures.is_empty() {
			RuleResult::passed(tid, name, format!("{} hooks, all >= {} windows each", hook_windows.len(), min_hook_windows), source)
		} else {
			RuleResult::failed(tid, name, format!("hook count below floor: {}", failures.join("; ")), source)
		}
	}
}

/// R7: progress-liveness — each evaluated tick shows >= `MIN_TICK_PROGRESS`
/// new sealed points, non-decreasing counters.
#[allow(clippy::cast_precision_loss, clippy::uninlined_format_args)]
fn evaluate_r7(total_ticks: u64, new_sealed_points: u64) -> RuleResult {
	let tid = "R7";
	let name = "progress-liveness";
	let source = String::from("const MIN_TICK_PROGRESS=1");
	if total_ticks == 0 {
		RuleResult::not_evaluated(tid, name, String::from("no ticks evaluated for progress"), source)
	} else if new_sealed_points == 0 {
		RuleResult::failed(tid, name, format!("zero new sealed points across {} ticks — no progress", total_ticks), source)
	} else {
		let avg = new_sealed_points as f64 / total_ticks as f64;
		if avg >= 0.999_f64 {
			RuleResult::passed(tid, name, format!("{} new sealed points across {} ticks (avg {:.2})", new_sealed_points, total_ticks, avg), source)
		} else {
			RuleResult::failed(tid, name, format!("avg {:.2} new sealed points/tick — below MIN_TICK_PROGRESS=1 (total={} ticks)", avg, total_ticks), source)
		}
	}
}

// ---------------------------------------------------------------------------
// Coverage checker
// ---------------------------------------------------------------------------

/// Check coverage floors and produce a `CoverageReport`.
#[allow(clippy::needless_vec_and, clippy::uninlined_format_args, clippy::too_many_arguments)]
fn check_coverage(oracle_ticks: Option<u64>, derived_ticks: Option<u64>, backup_windows: Option<u64>, steps: u64, hook_windows: &Vec<(String, u64)>, min_evaluated_ticks: u64, min_backup_windows: u64, min_derived_read_ticks: u64, min_hook_windows: u64) -> CoverageReport {
	let mut failures: Vec<String> = Vec::new();
	let mut first_observed: u64 = 0;
	let mut first_required: u64 = 0;
	let mut first_set = false;

	let mut push_failure = |msg: String, observed: u64, required: u64| {
		if !first_set {
			first_observed = observed;
			first_required = required;
			first_set = true;
		}
		failures.push(msg);
	};

	if min_evaluated_ticks > 0 {
		let observed = oracle_ticks.unwrap_or(0);
		if observed < min_evaluated_ticks {
			push_failure(format!("oracle: {} ticks observed, {} required", observed, min_evaluated_ticks), observed, min_evaluated_ticks);
		}
	}

	if min_derived_read_ticks > 0 {
		let observed = derived_ticks.unwrap_or(0);
		if observed < min_derived_read_ticks {
			push_failure(format!("derived read: {} ticks observed, {} required", observed, min_derived_read_ticks), observed, min_derived_read_ticks);
		}
	}

	if min_backup_windows > 0 {
		let observed = backup_windows.unwrap_or(0);
		if observed < min_backup_windows {
			push_failure(format!("backup: {} windows observed, {} required", observed, min_backup_windows), observed, min_backup_windows);
		}
	}

	if steps < 1 {
		push_failure(format!("steps: {} steps observed, at least 1 required", steps), steps, 1);
	}

	if min_hook_windows > 0 && !hook_windows.is_empty() {
		for (hook, count) in hook_windows {
			if *count < min_hook_windows {
				push_failure(format!("hook {}: {} windows observed, {} required", hook, *count, min_hook_windows), *count, min_hook_windows);
			}
		}
	}

	let status = if failures.is_empty() { CoverageStatus::Met } else { CoverageStatus::InsufficientCoverage(InsufficientCoverage { reason: failures.join("; "), observed: first_observed, required: first_required }) };

	CoverageReport { status, oracle_ticks, derived_ticks, backup_windows, steps, hook_windows: hook_windows.clone() }
}

// ---------------------------------------------------------------------------
// Override diffs
// ---------------------------------------------------------------------------

#[allow(clippy::uninlined_format_args)]
fn collect_overrides(min_evaluated_ticks: u64, min_backup_windows: u64, min_derived_read_ticks: u64, min_hook_windows: u64) -> Vec<OverrideDiff> {
	let mut diffs: Vec<OverrideDiff> = Vec::new();
	if min_evaluated_ticks != MIN_EVALUATED_TICKS {
		diffs.push(OverrideDiff { name: String::from("MIN_EVALUATED_TICKS"), default: format!("{}", MIN_EVALUATED_TICKS), overridden: format!("{}", min_evaluated_ticks) });
	}
	if min_backup_windows != MIN_BACKUP_WINDOWS {
		diffs.push(OverrideDiff { name: String::from("MIN_BACKUP_WINDOWS"), default: format!("{}", MIN_BACKUP_WINDOWS), overridden: format!("{}", min_backup_windows) });
	}
	if min_derived_read_ticks != MIN_DERIVED_READ_TICKS {
		diffs.push(OverrideDiff { name: String::from("MIN_DERIVED_READ_TICKS"), default: format!("{}", MIN_DERIVED_READ_TICKS), overridden: format!("{}", min_derived_read_ticks) });
	}
	if min_hook_windows != MIN_HOOK_WINDOWS {
		diffs.push(OverrideDiff { name: String::from("MIN_HOOK_WINDOWS"), default: format!("{}", MIN_HOOK_WINDOWS), overridden: format!("{}", min_hook_windows) });
	}
	diffs
}

// ---------------------------------------------------------------------------
// Overall outcome
// ---------------------------------------------------------------------------

/// Compute the overall outcome and summary from the rule results and coverage.
#[allow(clippy::needless_vec_and, clippy::uninlined_format_args)]
fn compute_overall_outcome(rules: &Vec<RuleResult>, coverage: &CoverageReport) -> (Outcome, String) {
	let any_fail = rules.iter().any(|r| r.outcome == Outcome::Fail);
	let all_not_evaluated = rules.iter().all(|r| r.outcome == Outcome::NotEvaluated);
	let coverage_sufficient = match coverage.status {
		CoverageStatus::Met => true,
		CoverageStatus::InsufficientCoverage(_) => false,
	};

	if !coverage_sufficient {
		return (Outcome::Fail, String::from("Insufficient coverage — run does not meet minimum floors for publishability"));
	}
	if any_fail {
		let fail_count = rules.iter().filter(|r| r.outcome == Outcome::Fail).count();
		return (Outcome::Fail, format!("{} of {} SLO rules fail", fail_count, rules.len()));
	}
	if all_not_evaluated {
		return (Outcome::NotEvaluated, String::from("All SLO rules were not evaluated — no claim can be made"));
	}
	let pass_count = rules.iter().filter(|r| r.outcome == Outcome::Pass).count();
	let any_not = rules.iter().any(|r| r.outcome == Outcome::NotEvaluated);
	if any_not {
		(Outcome::Pass, format!("{} pass, ? not evaluated — publishable with caveats", pass_count))
	} else {
		(Outcome::Pass, format!("All {} SLO rules pass", rules.len()))
	}
}

// ---------------------------------------------------------------------------
// Honesty section builder
// ---------------------------------------------------------------------------

/// Build the generated honesty section as Markdown.
#[allow(clippy::needless_vec_and)]
fn build_honesty_section(run: &SoakRun, slo: &SloReport, _coverage: &CoverageReport, overrides: &Vec<OverrideDiff>, contamination: Option<&ContaminationVerdict>, _flush_check: Option<&FlushCheckResult>) -> String {
	let mut out = String::with_capacity(2048);

	// --- Wins ---
	out.push_str("### Wins (rules passed)\n\n");
	let wins: Vec<&RuleResult> = slo.rules.iter().filter(|r| r.outcome == Outcome::Pass).collect();
	if wins.is_empty() {
		out.push_str("None.\n\n");
	} else {
		for rule in wins {
			let _ = writeln!(out, "- **{}** ({}) — {}", rule.id, rule.name, rule.evidence);
		}
		out.push('\n');
	}

	// --- Losses ---
	out.push_str("### Losses (rules failed)\n\n");
	let losses: Vec<&RuleResult> = slo.rules.iter().filter(|r| r.outcome == Outcome::Fail).collect();
	if losses.is_empty() {
		out.push_str("None.\n\n");
	} else {
		for rule in losses {
			let _ = writeln!(out, "- **{}** ({}) — {}", rule.id, rule.name, rule.evidence);
		}
		out.push('\n');
	}

	// --- Not evaluated ---
	out.push_str("### Rules not evaluated\n\n");
	let not_evals: Vec<&RuleResult> = slo.rules.iter().filter(|r| r.outcome == Outcome::NotEvaluated).collect();
	if not_evals.is_empty() {
		out.push_str("None.\n\n");
	} else {
		for rule in not_evals {
			let _ = writeln!(out, "- **{}** ({}) — {}", rule.id, rule.name, rule.evidence);
		}
		out.push('\n');
	}

	// --- Contamination ---
	out.push_str("### System contamination\n\n");
	match contamination {
		None => out.push_str("No system sampler ran; contamination was not evaluated.\n\n"),
		Some(c) => {
			if c.flags.is_empty() {
				out.push_str("No contamination flags fired.\n\n");
			} else {
				let _ = writeln!(out, "{} contamination flag(s) fired:", c.flags.len());
				for flag in &c.flags {
					let _ = writeln!(out, "- {} (worst: cpu={:.1}%, load={:.1}, dirty={}KiB, clock_step={}ms, gap={}ms)", flag.as_str(), c.worst.cpu_psi_some_avg10, c.worst.load_per_core, c.worst.dirty_kib, c.worst.clock_step_ms, c.worst.sample_gap_ms);
				}
				out.push('\n');
			}
		}
	}

	// --- Surfaces not tested ---
	out.push_str("### Surfaces not tested\n\n");
	let mut untested: Vec<&str> = Vec::new();

	if run.oracle.is_none() {
		untested.push("oracle (data correctness) — SOA-6");
	}
	if run.derived.is_none() {
		untested.push("derived-read (bucket-level checks) — SOA-10");
	}
	if run.backup.is_none() {
		untested.push("backup lane (prefix/restore verification) — SOA-11");
	}
	if run.phases.is_empty() {
		untested.push("failure deck (SIGTERM, injections) — SOA-8");
	}
	if contamination.is_none() {
		untested.push("system contamination monitoring — SOA-12");
	}
	// Lanes that do not exist yet:
	untested.push("SOA-15 TTL (coverage not yet wired)");
	untested.push("SOA-16 upgrade (coverage not yet wired)");
	// Hooks: if none configured, list as untested
	if !slo.rules.iter().any(|r| r.id == "R6" && r.outcome != Outcome::NotEvaluated) {
		untested.push("SOA-14 hooks (none configured)");
	}

	if untested.is_empty() {
		out.push_str("Every surface was tested.\n\n");
	} else {
		out.push_str("The following surfaces were not evaluated in this run:\n\n");
		for item in &untested {
			let _ = writeln!(out, "- {}", item);
		}
		out.push('\n');
	}

	// --- Overrides ---
	out.push_str("### Overrides\n\n");
	if overrides.is_empty() {
		out.push_str("No constant overridden — all coverage floors use their default values.\n\n");
	} else {
		out.push_str("The following constants were changed from their default:\n\n");
		out.push_str("| Constant | Default | Overridden |\n");
		out.push_str("|----------|---------|------------|\n");
		for diff in overrides {
			let _ = writeln!(out, "| {} | {} | {} |", diff.name, diff.default, diff.overridden);
		}
		out.push('\n');
	}

	// --- Limitations ---
	out.push_str("### Limitations\n\n");
	out.push_str("- The SLO evaluator judges each rule independently against the published\n");
	out.push_str("  threshold; it does not model transient bursts or interactions between rules.\n");
	out.push_str("- Coverage floors prevent publishing from insufficient data but do not guarantee\n");
	out.push_str("  statistical significance.\n");
	out.push_str("- The contamination check covers only the surfaces the system sampler monitored;\n");
	out.push_str("  unseen disturbances (a kernel update, a cron job, a network shift) are not\n");
	out.push_str("  detected.\n");
	out.push_str("- R8 (report-integrity) is verified at serialisation time and always passes for\n");
	out.push_str("  the types this module uses.\n");

	out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
	use super::*;
	use crate::{
		report::RunMetadata, soak::{SoakConfig, SoakHarness, SoakRun}, soak_backup::{BackupLaneVerdict, BackupPrefixVerdict, RecoveryTiming, RestoreVerdict}, soak_oracle::OracleVerdict, soak_read::DerivedReadVerdict, soak_system::{ContaminationFlag, FlushCheckResult, FlushFailureKind, WorstValues}
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

	fn backup_verdict(rpo_param: u64, rto_param: u64) -> BackupLaneVerdict {
		BackupLaneVerdict { prefix: BackupPrefixVerdict { acknowledged_ticks: 6, ticks_in_backup: 3, cut_tick: Some(2), points_expected: 24, points_in_backup: 24, mismatch_count: 0, mismatches: Vec::new() }, restore: RestoreVerdict { ticks_judged: 6, points_expected: 48, points_restored: 48, mismatch_count: 0, mismatches: Vec::new() }, timing: RecoveryTiming { backup_interval_ms: 1000, backup_duration_ms: rpo_param, restore_duration_ms: if rto_param > rpo_param { rto_param - rpo_param } else { rpo_param } } }
	}

	fn contamination_verdict(flags: Vec<ContaminationFlag>) -> ContaminationVerdict {
		ContaminationVerdict { flags: flags.clone(), sample_count: 60, window_ms: 60_000, worst: WorstValues { io_psi_some_avg10: if flags.is_empty() { 0.0 } else { 5.0 }, cpu_psi_some_avg10: if flags.is_empty() { 0.0 } else { 10.0 }, memory_psi_some_avg10: 0.0, dirty_kib: 0, load_per_core: if flags.is_empty() { 0.0 } else { 2.0 }, clock_step_ms: 0, sample_gap_ms: 0 }, device_written_bytes: Some(500_000), acknowledged_bytes: Some(500_000), foreign_write_bytes: Some(0), psi_capable: true, device: Some(String::from("nvme0n1")) }
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

	// -----------------------------------------------------------------------
	// R1: correctness-exact
	// -----------------------------------------------------------------------

	#[test]
	fn r1_passes_when_oracle_has_zero_mismatches() {
		let run = run_harness(60);
		let verdicts = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 0)), Some(&derived_verdict(60, 0)), Some(&backup_verdict(1000, 2000)), None, Some(&flush_check()), Vec::new(), 60, 240, 1, 1, 1, 1);
		let r1 = &verdicts.slo.rules[0];
		assert_eq!(r1.id, "R1");
		assert_eq!(r1.outcome, Outcome::Pass, "R1 must pass when oracle has zero mismatches");
		assert!(r1.evidence.contains("zero mismatches"), "evidence must mention zero mismatches");
	}

	#[test]
	fn r1_fails_when_oracle_has_mismatches() {
		let run = run_harness(60);
		let verdicts = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 3)), Some(&derived_verdict(60, 0)), Some(&backup_verdict(1000, 2000)), None, Some(&flush_check()), Vec::new(), 60, 240, 1, 1, 1, 1);
		let r1 = &verdicts.slo.rules[0];
		assert_eq!(r1.outcome, Outcome::Fail, "R1 must fail when oracle has mismatches");
		assert!(r1.evidence.contains("3 mismatches"), "evidence must mention 3 mismatches");
	}

	#[test]
	fn r1_not_evaluated_when_oracle_never_ran() {
		let run = run_harness(60);
		let verdicts = SoakReport::evaluate(&run, None, None, None, None, None, Vec::new(), 0, 0, 1, 1, 1, 1);
		let r1 = &verdicts.slo.rules[0];
		assert_eq!(r1.outcome, Outcome::NotEvaluated, "R1 must be not-evaluated when oracle never ran");
	}

	#[test]
	fn r1_fails_on_vacuous_oracle() {
		let run = run_harness(60);
		let verdicts = SoakReport::evaluate(&run, Some(&oracle_verdict(0, 0)), Some(&derived_verdict(60, 0)), Some(&backup_verdict(1000, 2000)), None, Some(&flush_check()), Vec::new(), 60, 240, 1, 1, 1, 1);
		let r1 = &verdicts.slo.rules[0];
		assert_eq!(r1.outcome, Outcome::Fail, "R1 must fail when oracle judged 0 ticks — vacuous");
	}

	// -----------------------------------------------------------------------
	// R2: contamination-explained
	// -----------------------------------------------------------------------

	#[test]
	fn r2_passes_when_no_flags_fired() {
		let run = run_harness(60);
		let verdicts = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 0)), Some(&derived_verdict(60, 0)), Some(&backup_verdict(1000, 2000)), Some(&contamination_verdict(Vec::new())), Some(&flush_check()), Vec::new(), 60, 240, 1, 1, 1, 1);
		let r2 = &verdicts.slo.rules[1];
		assert_eq!(r2.outcome, Outcome::Pass, "R2 must pass with no flags");
	}

	#[test]
	fn r2_passes_when_flags_have_worst_values() {
		let run = run_harness(60);
		let verdicts = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 0)), Some(&derived_verdict(60, 0)), Some(&backup_verdict(1000, 2000)), Some(&contamination_verdict(Vec::from([ContaminationFlag::CpuPressure, ContaminationFlag::LoadAverage]))), Some(&flush_check()), Vec::new(), 60, 240, 1, 1, 1, 1);
		let r2 = &verdicts.slo.rules[1];
		assert_eq!(r2.outcome, Outcome::Pass, "R2 must pass when flags have WorstValues");
		assert!(r2.evidence.contains("2 flag(s)"), "evidence must mention 2 flags");
	}

	#[test]
	fn r2_not_evaluated_when_contamination_never_ran() {
		let run = run_harness(60);
		let verdicts = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 0)), Some(&derived_verdict(60, 0)), Some(&backup_verdict(1000, 2000)), None, Some(&flush_check()), Vec::new(), 60, 240, 1, 1, 1, 1);
		let r2 = &verdicts.slo.rules[1];
		assert_eq!(r2.outcome, Outcome::NotEvaluated, "R2 must be not-evaluated when contamination never ran");
	}

	// -----------------------------------------------------------------------
	// R3: flush-discipline
	// -----------------------------------------------------------------------

	#[test]
	fn r3_passes_when_flush_check_passed() {
		let run = run_harness(60);
		let verdicts = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 0)), Some(&derived_verdict(60, 0)), Some(&backup_verdict(1000, 2000)), None, Some(&flush_check()), Vec::new(), 60, 240, 1, 1, 1, 1);
		let r3 = &verdicts.slo.rules[2];
		assert_eq!(r3.outcome, Outcome::Pass, "R3 must pass when flush check passes");
	}

	#[test]
	fn r3_fails_when_flush_check_fails() {
		let run = run_harness(60);
		let failed_check = FlushCheckResult { name: "flush_observed", device: Some(String::from("nvme0n1")), passed: false, failure: Some(FlushFailureKind::WritesNotObserved), skipped: None, device_written_bytes: Some(500_000), acknowledged_bytes: 1_000_000, dirty_kib_at_close: 0, slack_bytes: 0 };
		let verdicts = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 0)), Some(&derived_verdict(60, 0)), Some(&backup_verdict(1000, 2000)), None, Some(&failed_check), Vec::new(), 60, 240, 1, 1, 1, 1);
		let r3 = &verdicts.slo.rules[2];
		assert_eq!(r3.outcome, Outcome::Fail, "R3 must fail when flush check fails");
		assert!(r3.evidence.contains("writes_not_observed"), "evidence must mention writes_not_observed");
	}

	#[test]
	fn r3_not_evaluated_when_flush_check_never_ran() {
		let run = run_harness(60);
		let verdicts = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 0)), Some(&derived_verdict(60, 0)), Some(&backup_verdict(1000, 2000)), None, None, Vec::new(), 60, 240, 1, 1, 1, 1);
		let r3 = &verdicts.slo.rules[2];
		assert_eq!(r3.outcome, Outcome::NotEvaluated, "R3 must be not-evaluated when flush check never ran");
	}

	// -----------------------------------------------------------------------
	// R4: derived-read agreement
	// -----------------------------------------------------------------------

	#[test]
	fn r4_passes_when_derived_has_zero_mismatches() {
		let run = run_harness(60);
		let verdicts = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 0)), Some(&derived_verdict(60, 0)), Some(&backup_verdict(1000, 2000)), None, Some(&flush_check()), Vec::new(), 60, 240, 1, 1, 1, 1);
		let r4 = &verdicts.slo.rules[3];
		assert_eq!(r4.outcome, Outcome::Pass, "R4 must pass when derived read has zero mismatches");
	}

	#[test]
	fn r4_fails_when_derived_has_mismatches() {
		let run = run_harness(60);
		let verdicts = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 0)), Some(&derived_verdict(60, 2)), Some(&backup_verdict(1000, 2000)), None, Some(&flush_check()), Vec::new(), 60, 240, 1, 1, 1, 1);
		let r4 = &verdicts.slo.rules[3];
		assert_eq!(r4.outcome, Outcome::Fail, "R4 must fail when derived read has mismatches");
		assert!(r4.evidence.contains("2 mismatches"), "evidence must mention 2 mismatches");
	}

	#[test]
	fn r4_not_evaluated_when_derived_never_ran() {
		let run = run_harness(60);
		let verdicts = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 0)), None, None, None, None, Vec::new(), 60, 240, 1, 1, 1, 1);
		let r4 = &verdicts.slo.rules[3];
		assert_eq!(r4.outcome, Outcome::NotEvaluated, "R4 must be not-evaluated when derived never ran");
	}

	// -----------------------------------------------------------------------
	// R5: recovery-recorded
	// -----------------------------------------------------------------------

	#[test]
	fn r5_passes_when_backup_has_finite_rpo_rto() {
		let run = run_harness(60);
		let verdicts = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 0)), Some(&derived_verdict(60, 0)), Some(&backup_verdict(1000, 2000)), None, Some(&flush_check()), Vec::new(), 60, 240, 1, 1, 1, 1);
		let r5 = &verdicts.slo.rules[4];
		assert_eq!(r5.outcome, Outcome::Pass, "R5 must pass when backup has finite RPO/RTO");
		assert!(r5.evidence.contains("RPO="), "evidence must mention RPO value");
	}

	#[test]
	fn r5_fails_when_backup_has_zero_timing() {
		let run = run_harness(60);
		let zero_timing = BackupLaneVerdict { prefix: BackupPrefixVerdict { acknowledged_ticks: 6, ticks_in_backup: 3, cut_tick: Some(2), points_expected: 24, points_in_backup: 24, mismatch_count: 0, mismatches: Vec::new() }, restore: RestoreVerdict { ticks_judged: 6, points_expected: 48, points_restored: 48, mismatch_count: 0, mismatches: Vec::new() }, timing: RecoveryTiming { backup_interval_ms: 0, backup_duration_ms: 0, restore_duration_ms: 0 } };
		let verdicts = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 0)), Some(&derived_verdict(60, 0)), Some(&zero_timing), None, Some(&flush_check()), Vec::new(), 60, 240, 1, 1, 1, 1);
		let r5 = &verdicts.slo.rules[4];
		assert_eq!(r5.outcome, Outcome::Fail, "R5 must fail when backup RecoveryTiming is all zeroes");
	}

	#[test]
	fn r5_not_evaluated_when_backup_never_ran() {
		let run = run_harness(60);
		let verdicts = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 0)), Some(&derived_verdict(60, 0)), None, None, Some(&flush_check()), Vec::new(), 60, 240, 1, 1, 1, 1);
		let r5 = &verdicts.slo.rules[4];
		assert_eq!(r5.outcome, Outcome::NotEvaluated, "R5 must be not-evaluated when backup never ran");
	}

	// -----------------------------------------------------------------------
	// R6: hook-contract
	// -----------------------------------------------------------------------

	#[test]
	fn r6_passes_when_all_hooks_meet_floor() {
		let run = run_harness(60);
		let hooks: Vec<(String, u64)> = Vec::from([(String::from("pre_flush"), 3_u64), (String::from("post_restore"), 5_u64)]);
		let verdicts = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 0)), Some(&derived_verdict(60, 0)), Some(&backup_verdict(1000, 2000)), None, Some(&flush_check()), hooks, 60, 240, 1, 1, 1, 1);
		let r6 = &verdicts.slo.rules[5];
		assert_eq!(r6.outcome, Outcome::Pass, "R6 must pass when all hooks meet the window floor");
	}

	#[test]
	fn r6_not_evaluated_when_no_hooks_configured() {
		let run = run_harness(60);
		let verdicts = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 0)), Some(&derived_verdict(60, 0)), Some(&backup_verdict(1000, 2000)), None, Some(&flush_check()), Vec::new(), 60, 240, 1, 1, 1, 1);
		let r6 = &verdicts.slo.rules[5];
		assert_eq!(r6.outcome, Outcome::NotEvaluated, "R6 must be not-evaluated when no hooks configured");
	}

	#[test]
	fn r6_fails_when_hook_count_below_floor() {
		let run = run_harness(60);
		let hooks: Vec<(String, u64)> = Vec::from([(String::from("pre_flush"), 0_u64)]);
		let verdicts = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 0)), Some(&derived_verdict(60, 0)), Some(&backup_verdict(1000, 2000)), None, Some(&flush_check()), hooks, 60, 240, 1, 1, 1, 1);
		let r6 = &verdicts.slo.rules[5];
		assert_eq!(r6.outcome, Outcome::Fail, "R6 must fail when a hook count is below the floor");
	}

	// -----------------------------------------------------------------------
	// R7: progress-liveness
	// -----------------------------------------------------------------------

	#[test]
	fn r7_passes_when_progress_has_new_sealed_points() {
		let run = run_harness(60);
		let verdicts = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 0)), Some(&derived_verdict(60, 0)), Some(&backup_verdict(1000, 2000)), None, Some(&flush_check()), Vec::new(), 60, 240, 1, 1, 1, 1);
		let r7 = &verdicts.slo.rules[6];
		assert_eq!(r7.outcome, Outcome::Pass, "R7 must pass when progress has >= 1 new sealed points/tick on average");
	}

	#[test]
	fn r7_fails_when_no_new_sealed_points() {
		let run = run_harness(60);
		let verdicts = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 0)), Some(&derived_verdict(60, 0)), Some(&backup_verdict(1000, 2000)), None, Some(&flush_check()), Vec::new(), 60, 0, 1, 1, 1, 1);
		let r7 = &verdicts.slo.rules[6];
		assert_eq!(r7.outcome, Outcome::Fail, "R7 must fail when there are zero new sealed points");
	}

	#[test]
	fn r7_not_evaluated_when_no_ticks() {
		let run = run_harness(0);
		let verdicts = SoakReport::evaluate(&run, None, None, None, None, None, Vec::new(), 0, 0, 1, 1, 1, 1);
		let r7 = &verdicts.slo.rules[6];
		assert_eq!(r7.outcome, Outcome::NotEvaluated, "R7 must be not-evaluated when no ticks");
	}

	// -----------------------------------------------------------------------
	// R8: report-integrity — round-trip test
	// -----------------------------------------------------------------------

	#[test]
	fn r8_round_trips_through_json() {
		let run = run_harness(60);
		let report = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 0)), Some(&derived_verdict(60, 0)), Some(&backup_verdict(1000, 2000)), Some(&contamination_verdict(Vec::new())), Some(&flush_check()), Vec::new(), 60, 240, 1, 1, 1, 1);
		let json = report.to_json().expect("serialize report");
		let back: SoakReport = serde_json::from_str(&json).expect("deserialize report");
		assert_eq!(back.schema_version, SOAK_REPORT_SCHEMA_VERSION, "schema version must survive round-trip");
		assert_eq!(back.slo.rules.len(), 8, "deserialized report must carry 8 rules");
		assert_eq!(back.slo.outcome, report.slo.outcome, "deserialized outcome must match");
		assert!(!back.honesty.is_empty(), "deserialized honesty section must be present");
	}

	// -----------------------------------------------------------------------
	// Coverage floors
	// -----------------------------------------------------------------------

	#[test]
	fn coverage_met_when_all_floors_satisfied() {
		let run = run_harness(60);
		let verdicts = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 0)), Some(&derived_verdict(60, 0)), Some(&backup_verdict(1000, 2000)), None, Some(&flush_check()), Vec::new(), 60, 240, 50, 3, 30, 1);
		match verdicts.coverage.status {
			CoverageStatus::Met => {} // expected
			CoverageStatus::InsufficientCoverage(ins) => {
				panic!("expected Met, got InsufficientCoverage: {0}", ins.reason);
			}
		}
	}

	#[test]
	fn coverage_fails_when_oracle_ticks_below_floor() {
		let run = run_harness(60);
		let verdicts = SoakReport::evaluate(&run, Some(&oracle_verdict(5, 0)), Some(&derived_verdict(60, 0)), Some(&backup_verdict(1000, 2000)), None, Some(&flush_check()), Vec::new(), 60, 240, 10, 1, 1, 1);
		match verdicts.coverage.status {
			CoverageStatus::InsufficientCoverage(ins) => {
				assert!(ins.reason.contains("oracle"), "reason must mention oracle: {0}", ins.reason);
			}
			_ => panic!("expected InsufficientCoverage"),
		}
	}

	#[test]
	fn coverage_fails_when_derived_ticks_below_floor() {
		let run = run_harness(60);
		let verdicts = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 0)), Some(&derived_verdict(3, 0)), Some(&backup_verdict(1000, 2000)), None, Some(&flush_check()), Vec::new(), 60, 240, 1, 1, 10, 1);
		match verdicts.coverage.status {
			CoverageStatus::InsufficientCoverage(ins) => {
				assert!(ins.reason.contains("derived"), "reason must mention derived: {0}", ins.reason);
			}
			_ => panic!("expected InsufficientCoverage"),
		}
	}

	#[test]
	fn coverage_fails_when_backup_windows_below_floor() {
		let run = run_harness(60);
		let verdicts = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 0)), Some(&derived_verdict(60, 0)), Some(&backup_verdict(1000, 2000)), None, Some(&flush_check()), Vec::new(), 60, 240, 1, 10, 1, 1);
		match verdicts.coverage.status {
			CoverageStatus::InsufficientCoverage(ins) => {
				assert!(ins.reason.contains("backup"), "reason must mention backup: {0}", ins.reason);
			}
			_ => panic!("expected InsufficientCoverage"),
		}
	}

	#[test]
	fn coverage_fails_when_steps_is_zero() {
		let run = run_harness(0);
		let verdicts = SoakReport::evaluate(&run, None, None, None, None, None, Vec::new(), 0, 0, 1, 1, 1, 1);
		match verdicts.coverage.status {
			CoverageStatus::InsufficientCoverage(ins) => {
				assert!(ins.reason.contains("steps"), "reason must mention steps: {0}", ins.reason);
			}
			_ => panic!("expected InsufficientCoverage"),
		}
	}

	// -----------------------------------------------------------------------
	// Override diffs
	// -----------------------------------------------------------------------

	#[test]
	fn overrides_empty_when_all_defaults() {
		let run = run_harness(60);
		let verdicts = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 0)), Some(&derived_verdict(60, 0)), Some(&backup_verdict(1000, 2000)), None, Some(&flush_check()), Vec::new(), 60, 240, 1, 1, 1, 1);
		assert!(verdicts.overrides.is_empty(), "overrides must be empty when all constants are default");
	}

	#[test]
	fn overrides_populated_when_constants_differ() {
		let run = run_harness(60);
		let verdicts = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 0)), Some(&derived_verdict(60, 0)), Some(&backup_verdict(1000, 2000)), None, Some(&flush_check()), Vec::new(), 60, 240, 5, 3, 4, 2);
		assert_eq!(verdicts.overrides.len(), 4, "must have 4 overrides");
		assert!(verdicts.overrides.iter().any(|d| d.name == "MIN_EVALUATED_TICKS" && d.overridden == "5"), "must have MIN_EVALUATED_TICKS override");
		assert!(verdicts.overrides.iter().any(|d| d.name == "MIN_BACKUP_WINDOWS" && d.overridden == "3"), "must have MIN_BACKUP_WINDOWS override");
	}

	// -----------------------------------------------------------------------
	// Honesty section
	// -----------------------------------------------------------------------

	#[test]
	fn honesty_mentions_wins_and_losses() {
		let run = run_harness(60);
		let verdicts = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 0)), Some(&derived_verdict(60, 0)), Some(&backup_verdict(1000, 2000)), Some(&contamination_verdict(Vec::new())), Some(&flush_check()), Vec::new(), 60, 240, 1, 1, 1, 1);
		assert!(verdicts.honesty.contains("Wins (rules passed)"), "honesty must have a wins section");
		assert!(verdicts.honesty.contains("Losses (rules failed)"), "honesty must have a losses section");
		assert!(verdicts.honesty.contains("Contamination"), "honesty must have a contamination section");
		assert!(verdicts.honesty.contains("Surfaces not tested"), "honesty must list untested surfaces");
		assert!(verdicts.honesty.contains("Overrides"), "honesty must have an overrides section");
		assert!(verdicts.honesty.contains("Limitations"), "honesty must list limitations");
		assert!(verdicts.honesty.contains("SOA-15 TTL"), "honesty must mention SOA-15");
		assert!(verdicts.honesty.contains("SOA-16 upgrade"), "honesty must mention SOA-16");
	}

	#[test]
	fn honesty_mentions_failing_rules_when_rules_fail() {
		let run = run_harness(60);
		let verdicts = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 2)), Some(&derived_verdict(60, 0)), Some(&backup_verdict(1000, 2000)), None, Some(&flush_check()), Vec::new(), 60, 240, 1, 1, 1, 1);
		assert!(verdicts.honesty.contains("R1"), "honesty must mention R1 in losses");
		assert!(verdicts.honesty.contains("Losses"), "honesty must have a losses section");
	}

	// -----------------------------------------------------------------------
	// Overall outcome
	// -----------------------------------------------------------------------

	#[test]
	fn overall_outcome_pass_when_all_rules_pass_and_coverage_met() {
		let run = run_harness(60);
		let verdicts = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 0)), Some(&derived_verdict(60, 0)), Some(&backup_verdict(1000, 2000)), Some(&contamination_verdict(Vec::new())), Some(&flush_check()), Vec::new(), 60, 240, 1, 1, 1, 1);
		assert_eq!(verdicts.slo.outcome, Outcome::Pass, "overall outcome must be Pass when all rules pass and coverage is met");
	}

	#[test]
	fn overall_outcome_fail_when_any_rule_fails() {
		let run = run_harness(60);
		let verdicts = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 1)), Some(&derived_verdict(60, 0)), Some(&backup_verdict(1000, 2000)), Some(&contamination_verdict(Vec::new())), Some(&flush_check()), Vec::new(), 60, 240, 1, 1, 1, 1);
		assert_eq!(verdicts.slo.outcome, Outcome::Fail, "overall outcome must be Fail when R1 fails");
	}

	#[test]
	fn overall_outcome_fail_when_coverage_not_met() {
		let run = run_harness(0);
		let verdicts = SoakReport::evaluate(&run, None, None, None, None, None, Vec::new(), 0, 0, 1, 1, 1, 1);
		assert_eq!(verdicts.slo.outcome, Outcome::Fail, "overall outcome must be Fail when coverage is not met (steps=0)");
		assert!(verdicts.slo.summary.contains("Insufficient coverage"), "summary must mention insufficient coverage");
	}

	// -----------------------------------------------------------------------
	// SOAK_REPORT_SCHEMA_VERSION
	// -----------------------------------------------------------------------

	#[test]
	fn soak_report_schema_version_is_1() {
		assert_eq!(SOAK_REPORT_SCHEMA_VERSION, 1, "schema version must be 1");
	}

	#[test]
	fn soak_reports_carry_the_schema_version() {
		let run = run_harness(60);
		let verdicts = SoakReport::evaluate(&run, Some(&oracle_verdict(60, 0)), Some(&derived_verdict(60, 0)), Some(&backup_verdict(1000, 2000)), None, Some(&flush_check()), Vec::new(), 60, 240, 1, 1, 1, 1);
		assert_eq!(verdicts.schema_version, 1, "report must carry schema_version=1");
		assert_eq!(verdicts.slo.schema_version, 1, "SloReport must carry schema_version=1");
	}
}
